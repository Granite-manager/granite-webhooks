//! The Stripe REST API (the calls billing makes), webhook signature checks,
//! and Stripe objects read into billing snapshots.
//!
//! Requests pin the API version the CRM's Stripe SDK used, so objects have
//! the same shape (`invoice.parent.subscription_details`, periods on
//! subscription items, `invoice.confirmation_secret`).

use super::{BillingEvent, InvoiceSnapshot, SubscriptionSnapshot};
use chrono::{DateTime, NaiveDateTime};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

const API_BASE: &str = "https://api.stripe.com";
/// `stripe` npm 22's `ApiVersion`.
pub const API_VERSION: &str = "2026-08-26.dahlia";
/// The version sent to Stripe's `/v2` routes (Accounts v2).
const API_V2_VERSION: &str = "2026-10-28.endive";
/// How old a webhook signature may be (the SDK's default).
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
/// What a customer's card Checkout page says by its Pay button.
pub const US_CARDS_ONLY: &str = "We accept cards issued in the United States only.";

#[derive(Debug, thiserror::Error)]
pub enum StripeError {
    #[error("Billing is not configured")]
    NotConfigured,
    /// Stripe refused the request; the message is Stripe's, for the user
    /// (e.g. "Your card was declined.").
    #[error("{0}")]
    Api(String),
    #[error("Stripe request failed: {0}")]
    Request(String),
}

/// A Stripe client; disabled without `STRIPE_SECRET_KEY`.
#[derive(Clone)]
pub struct Stripe {
    key: Option<Arc<str>>,
    base: Arc<str>,
    http: reqwest::Client,
    /// Pause between polls of a subscription that is not paid yet.
    pub poll_delay: Duration,
}

impl Stripe {
    pub fn disabled() -> Self {
        Self::new(None, API_BASE)
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var("STRIPE_SECRET_KEY").ok().as_deref(), API_BASE)
    }

    /// `base` is `https://api.stripe.com` (a stub in tests).
    pub fn new(key: Option<&str>, base: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            base: Arc::from(base.trim_end_matches('/')),
            http: common::http::client(),
            poll_delay: Duration::from_millis(1500),
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(&str, String)],
    ) -> Result<Value, StripeError> {
        self.send_as(None, method, path, form).await
    }

    /// `account`: the connected account the request acts on (Stripe Connect's
    /// `Stripe-Account` header); `None` for the platform's own account.
    async fn send_as(
        &self,
        account: Option<&str>,
        method: reqwest::Method,
        path: &str,
        form: &[(&str, String)],
    ) -> Result<Value, StripeError> {
        self.send_once(account, None, method, path, form).await
    }

    /// `idempotency`: Stripe's `Idempotency-Key`: a request sent again with
    /// the same key within 24 hours gets the first one's answer, so it is
    /// made once.
    async fn send_once(
        &self,
        account: Option<&str>,
        idempotency: Option<&str>,
        method: reqwest::Method,
        path: &str,
        form: &[(&str, String)],
    ) -> Result<Value, StripeError> {
        let key = self.key.as_ref().ok_or(StripeError::NotConfigured)?;
        // A request meant for a connected account never goes out without
        // one: Stripe would run it on the platform's own account, and a
        // customer's payment would land there.
        if account.is_some_and(|account| !account.starts_with("acct_")) {
            return Err(StripeError::Request(
                "no connected Stripe account to act on".into(),
            ));
        }
        let encoded = serde_html_form::to_string(form)
            .map_err(|err| StripeError::Request(err.to_string()))?;
        let get = method == reqwest::Method::GET;
        let url = if get && !encoded.is_empty() {
            format!("{}{path}?{encoded}", self.base)
        } else {
            format!("{}{path}", self.base)
        };
        let request = self
            .http
            .request(method, &url)
            .bearer_auth(&**key)
            .header("Stripe-Version", API_VERSION)
            .timeout(Duration::from_secs(30));
        let request = match account {
            Some(account) => request.header("Stripe-Account", account),
            None => request,
        };
        let request = match idempotency {
            Some(idempotency) => request.header("Idempotency-Key", idempotency),
            None => request,
        };
        let request = if get {
            request
        } else {
            request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(encoded)
        };
        let response = request
            .send()
            .await
            .map_err(|err| StripeError::Request(err.without_url().to_string()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|err| StripeError::Request(err.without_url().to_string()))?;
        if status.is_success() {
            Ok(body)
        } else {
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("Stripe request failed")
                .to_string();
            Err(StripeError::Api(message))
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, StripeError> {
        self.send(reqwest::Method::GET, path, query).await
    }

    async fn post(&self, path: &str, form: &[(&str, String)]) -> Result<Value, StripeError> {
        self.send(reqwest::Method::POST, path, form).await
    }

    /// The customer's subscriptions, any status, newest first (up to 20).
    pub async fn list_subscriptions(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/subscriptions",
                &[
                    ("customer", customer.into()),
                    ("status", "all".into()),
                    ("limit", "20".into()),
                ],
            )
            .await?;
        Ok(data(list))
    }

    pub async fn subscription(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/subscriptions/{}", path_part(id)), &[])
            .await
    }

    pub async fn cancel_subscription(&self, id: &str) -> Result<Value, StripeError> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/v1/subscriptions/{}", path_part(id)),
            &[],
        )
        .await
    }

    /// A subscription that waits for the first payment, with the invoice's
    /// confirmation secret for the browser.
    pub async fn create_subscription(
        &self,
        customer: &str,
        price_id: &str,
        quantity: i64,
        payment_method: &str,
        company_id: i32,
    ) -> Result<Value, StripeError> {
        self.post(
            "/v1/subscriptions",
            &[
                ("customer", customer.into()),
                ("items[0][price]", price_id.into()),
                ("items[0][quantity]", quantity.to_string()),
                ("default_payment_method", payment_method.into()),
                ("payment_behavior", "default_incomplete".into()),
                (
                    "payment_settings[save_default_payment_method]",
                    "on_subscription".into(),
                ),
                ("metadata[company_id]", company_id.to_string()),
                ("expand[]", "latest_invoice.confirmation_secret".into()),
            ],
        )
        .await
    }

    pub async fn invoice(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/invoices/{}", path_part(id)), &[])
            .await
    }

    /// Open invoices of the customer (up to 100).
    pub async fn open_invoices(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/invoices",
                &[
                    ("customer", customer.into()),
                    ("status", "open".into()),
                    ("limit", "100".into()),
                ],
            )
            .await?;
        Ok(data(list))
    }

    pub async fn payment_method(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/payment_methods/{}", path_part(id)), &[])
            .await
    }

    pub async fn attach_payment_method(
        &self,
        id: &str,
        customer: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/payment_methods/{}/attach", path_part(id)),
            &[("customer", customer.into())],
        )
        .await
    }

    pub async fn set_default_payment_method(
        &self,
        customer: &str,
        payment_method: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/customers/{}", path_part(customer)),
            &[(
                "invoice_settings[default_payment_method]",
                payment_method.into(),
            )],
        )
        .await
    }
}

impl Stripe {
    /// The customer's latest invoices (up to 24), any status.
    pub async fn list_invoices(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/invoices",
                &[("customer", customer.into()), ("limit", "24".into())],
            )
            .await?;
        Ok(data(list))
    }

    /// A customer for a company that never subscribed.
    pub async fn create_customer(&self, name: &str, company_id: i32) -> Result<Value, StripeError> {
        self.post(
            "/v1/customers",
            &[
                ("name", name.into()),
                ("metadata[company_id]", company_id.to_string()),
            ],
        )
        .await
    }

    /// A Checkout page that starts a subscription to `price_id`; at most one
    /// such page is open per customer (`subscription_checkout`).
    pub async fn create_checkout_session(
        &self,
        customer: &str,
        company_id: i32,
        price_id: &str,
        quantity: i64,
        return_base: &str,
    ) -> Result<Value, StripeError> {
        self.subscription_checkout(
            customer,
            vec![
                ("mode", "subscription".into()),
                ("customer", customer.into()),
                ("client_reference_id", company_id.to_string()),
                ("line_items[0][price]", price_id.into()),
                ("line_items[0][quantity]", quantity.to_string()),
                ("success_url", format!("{return_base}?checkout=success")),
                ("cancel_url", format!("{return_base}?checkout=cancel")),
                ("metadata[company_id]", company_id.to_string()),
                (
                    "subscription_data[metadata][company_id]",
                    company_id.to_string(),
                ),
            ],
        )
        .await
    }

    /// The customer's open Checkout pages that would start a subscription
    /// (up to 100, newest first).
    pub async fn open_subscription_checkouts(
        &self,
        customer: &str,
    ) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                "/v1/checkout/sessions",
                &[
                    ("customer", customer.into()),
                    ("status", "open".into()),
                    ("limit", "100".into()),
                ],
            )
            .await?;
        Ok(data(list)
            .into_iter()
            .filter(|session| session["mode"] == "subscription")
            .collect())
    }

    /// Closes an open Checkout page of the platform's own account, so nobody
    /// can pay on it any more.
    pub async fn expire_subscription_checkout(&self, id: &str) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/checkout/sessions/{}/expire", path_part(id)),
            &[],
        )
        .await
    }

    /// A Checkout page that starts a subscription for `customer`, with at
    /// most one such page open at a time, so a company cannot pay for two
    /// subscriptions from two pages (two tabs, a second click, a link opened
    /// again). The open page made by the very same request (same price, same
    /// return address, same first charge) is handed back; any other open one
    /// is closed first. A page that cannot be closed (it was just paid)
    /// fails the call: the caller's check for a running subscription
    /// answers the next try.
    async fn subscription_checkout(
        &self,
        customer: &str,
        mut form: Vec<(&str, String)>,
    ) -> Result<Value, StripeError> {
        let request = form_digest(&form);
        let mut same: Option<Value> = None;
        for session in self.open_subscription_checkouts(customer).await? {
            let reusable = same.is_none()
                && session["metadata"][SUBSCRIPTION_CHECKOUT_REQUEST] == request.as_str()
                && session["url"].is_string();
            if reusable {
                same = Some(session);
            } else if let Some(id) = session["id"].as_str() {
                self.expire_subscription_checkout(id).await?;
            }
        }
        if let Some(session) = same {
            return Ok(session);
        }
        form.push((SUBSCRIPTION_CHECKOUT_REQUEST_FIELD, request));
        self.post("/v1/checkout/sessions", &form).await
    }

    /// A billing portal page; `payment_method_update` opens the card form.
    pub async fn create_portal_session(
        &self,
        customer: &str,
        return_url: &str,
        payment_method_update: bool,
    ) -> Result<Value, StripeError> {
        let mut form = vec![
            ("customer", customer.to_string()),
            ("return_url", return_url.to_string()),
        ];
        if payment_method_update {
            form.push(("flow_data[type]", "payment_method_update".into()));
        }
        self.post("/v1/billing_portal/sessions", &form).await
    }

    /// Moves the subscription's item to `price_id` (prorated).
    pub async fn update_subscription(
        &self,
        id: &str,
        item_id: &str,
        price_id: &str,
        quantity: i64,
        company_id: i32,
    ) -> Result<Value, StripeError> {
        self.post(
            &format!("/v1/subscriptions/{}", path_part(id)),
            &[
                ("items[0][id]", item_id.into()),
                ("items[0][price]", price_id.into()),
                ("items[0][quantity]", quantity.to_string()),
                ("proration_behavior", "create_prorations".into()),
                ("metadata[company_id]", company_id.to_string()),
            ],
        )
        .await
    }

    /// The customer's saved cards (up to 10).
    pub async fn list_cards(&self, customer: &str) -> Result<Vec<Value>, StripeError> {
        let list = self
            .get(
                &format!("/v1/customers/{}/payment_methods", path_part(customer)),
                &[("type", "card".into()), ("limit", "10".into())],
            )
            .await?;
        Ok(data(list))
    }
}

/// What the payments page (`payments.granite-manager.com`) needs: one monthly
/// subscription per company at the company's own price.
impl Stripe {
    /// Makes sure the product with this fixed `id` exists with `tax_code`,
    /// creating it the first time. Prices are set per checkout, so one
    /// product is enough. Checkout refuses a product without a tax code on
    /// accounts with Managed Payments (Stripe as merchant of record).
    pub async fn ensure_product(
        &self,
        id: &str,
        name: &str,
        tax_code: &str,
    ) -> Result<(), StripeError> {
        let path = format!("/v1/products/{}", path_part(id));
        let product = match self.get(&path, &[]).await {
            Ok(product) => product,
            Err(_) => {
                let created = self
                    .post(
                        "/v1/products",
                        &[
                            ("id", id.into()),
                            ("name", name.into()),
                            ("tax_code", tax_code.into()),
                        ],
                    )
                    .await;
                match created {
                    Ok(_) => return Ok(()),
                    // Another request created it in the meantime.
                    Err(error) => self.get(&path, &[]).await.map_err(|_| error)?,
                }
            }
        };
        if stripe_id(&product["tax_code"]).is_none() {
            self.post(&path, &[("tax_code", tax_code.into())]).await?;
        }
        Ok(())
    }

    /// A Checkout page that starts a monthly subscription to `product` at
    /// `unit_amount_cents` (USD), for the company's existing customer. With
    /// `first_payment_at` (unix seconds) the card is saved now and first
    /// charged then, then every month from that day. At most one such page
    /// is open per customer (`subscription_checkout`).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_monthly_checkout_session(
        &self,
        customer: &str,
        company_id: i32,
        product: &str,
        unit_amount_cents: i64,
        success_url: &str,
        cancel_url: &str,
        first_payment_at: Option<i64>,
    ) -> Result<Value, StripeError> {
        let mut form: Vec<(&str, String)> = vec![
            ("mode", "subscription".into()),
            ("customer", customer.into()),
            ("client_reference_id", company_id.to_string()),
            ("line_items[0][quantity]", "1".into()),
            ("line_items[0][price_data][currency]", "usd".into()),
            ("line_items[0][price_data][product]", product.into()),
            (
                "line_items[0][price_data][unit_amount]",
                unit_amount_cents.to_string(),
            ),
            (
                "line_items[0][price_data][recurring][interval]",
                "month".into(),
            ),
            ("success_url", success_url.into()),
            ("cancel_url", cancel_url.into()),
            ("metadata[company_id]", company_id.to_string()),
            (
                "subscription_data[metadata][company_id]",
                company_id.to_string(),
            ),
        ];
        if let Some(at) = first_payment_at {
            form.push(("subscription_data[trial_end]", at.to_string()));
        }
        self.subscription_checkout(customer, form).await
    }

    /// A billing portal page (cancel, payment method, invoices) with the
    /// given portal configuration, or the account's default without one.
    pub async fn create_portal_session_with(
        &self,
        customer: &str,
        return_url: &str,
        configuration: Option<&str>,
    ) -> Result<Value, StripeError> {
        let mut form = vec![
            ("customer", customer.to_string()),
            ("return_url", return_url.to_string()),
        ];
        if let Some(configuration) = configuration {
            form.push(("configuration", configuration.to_string()));
        }
        self.post("/v1/billing_portal/sessions", &form).await
    }

    /// The portal configuration to open the payments page's customers with:
    /// `None` when the account has a default one (saved in the Dashboard),
    /// else the one tagged `metadata[granite]=payments`, created the first
    /// time. So a new Stripe account needs no Dashboard setup.
    pub async fn payments_portal_configuration(&self) -> Result<Option<String>, StripeError> {
        let defaults = self
            .get(
                "/v1/billing_portal/configurations",
                &[("is_default", "true".into()), ("limit", "1".into())],
            )
            .await?;
        if !data(defaults).is_empty() {
            return Ok(None);
        }
        let active = self
            .get(
                "/v1/billing_portal/configurations",
                &[("active", "true".into()), ("limit", "100".into())],
            )
            .await?;
        if let Some(id) = data(active)
            .iter()
            .find(|config| config["metadata"]["granite"] == "payments")
            .and_then(|config| config["id"].as_str())
        {
            return Ok(Some(id.to_string()));
        }
        let created = self
            .post(
                "/v1/billing_portal/configurations",
                &[
                    ("metadata[granite]", "payments".into()),
                    ("features[invoice_history][enabled]", "true".into()),
                    ("features[payment_method_update][enabled]", "true".into()),
                    ("features[customer_update][enabled]", "true".into()),
                    (
                        "features[customer_update][allowed_updates][0]",
                        "email".into(),
                    ),
                    (
                        "features[customer_update][allowed_updates][1]",
                        "address".into(),
                    ),
                    ("features[subscription_cancel][enabled]", "true".into()),
                    (
                        "features[subscription_cancel][mode]",
                        "at_period_end".into(),
                    ),
                ],
            )
            .await?;
        Ok(created["id"].as_str().map(str::to_string))
    }
}

/// On a Checkout page that starts a subscription: the digest of the request
/// it was made with (`form_digest`), in its metadata. A later request for
/// the very same page gets that page back instead of a second one.
const SUBSCRIPTION_CHECKOUT_REQUEST: &str = "granite_request";
const SUBSCRIPTION_CHECKOUT_REQUEST_FIELD: &str = "metadata[granite_request]";

/// The first 16 hex characters of the SHA-256 of a request's form, for an
/// `Idempotency-Key` that changes when the request does, and to tell whether
/// two requests ask for the same thing.
fn form_digest(form: &[(&str, String)]) -> String {
    use sha2::Digest;
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    for (key, value) in form {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// What a customer pays a company on Stripe Checkout
/// ([`Stripe::create_payment_checkout_session`]).
pub struct PaymentCheckout<'a> {
    /// Our id of the payment, saved on the session and its payment intent
    /// (`metadata[granite_payment]`), so webhooks find the payment.
    pub payment_id: &'a str,
    /// `card` or `us_bank_account` (ACH): the one method the page offers.
    pub payment_method: &'a str,
    /// The line the customer reads, e.g. the job's name.
    pub description: &'a str,
    /// What goes toward the job, in US cents.
    pub amount_cents: i64,
    /// The customer's part of the fee, a second line when above zero.
    pub customer_fee_cents: i64,
    /// That line's name, e.g. `Processing fee (0.5%)`.
    pub customer_fee_name: &'a str,
    /// More metadata for the session and its payment intent, e.g.
    /// `("company_id", "100")`: who and what the payment is for.
    pub metadata: &'a [(&'a str, String)],
    /// What the platform keeps of the payment, when above zero.
    pub application_fee_cents: i64,
    pub success_url: &'a str,
    pub cancel_url: &'a str,
    /// The start of Stripe's `Idempotency-Key` for the request; a digest of
    /// what is sent is added to it. Two requests with the same key and the
    /// same content make one session.
    pub idempotency_key: &'a str,
}

/// Stripe Connect: a company's customers pay the company on its own
/// connected account (direct charges), and the platform takes an application
/// fee. Requests about a connected account's objects carry `Stripe-Account`.
impl Stripe {
    /// A connected account for the company, of the kind Accounts v1 called
    /// Standard: the company has Stripe's full dashboard, Stripe charges its
    /// fees to the company and handles its disputes. It may take card and ACH
    /// payments once Stripe has its details. Made with Accounts v2, which
    /// Stripe requires of new platforms (`POST /v1/accounts` is refused); the
    /// `/v1` routes read and charge the account all the same.
    pub async fn create_connected_account(
        &self,
        name: &str,
        company_id: i32,
    ) -> Result<Value, StripeError> {
        self.post_v2(
            "/v2/core/accounts",
            &serde_json::json!({
                "display_name": name,
                "identity": { "country": "us" },
                "dashboard": "full",
                "defaults": {
                    "responsibilities": {
                        "fees_collector": "stripe",
                        "losses_collector": "stripe",
                    },
                },
                "configuration": {
                    "merchant": {
                        "capabilities": {
                            "card_payments": { "requested": true },
                            "ach_debit_payments": { "requested": true },
                        },
                    },
                },
                "metadata": { "company_id": company_id.to_string() },
            }),
        )
        .await
    }

    /// A request to Stripe's `/v2` API, which takes JSON and its own version.
    async fn post_v2(&self, path: &str, body: &Value) -> Result<Value, StripeError> {
        let key = self.key.as_ref().ok_or(StripeError::NotConfigured)?;
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&**key)
            .header("Stripe-Version", API_V2_VERSION)
            .timeout(Duration::from_secs(30))
            .json(body)
            .send()
            .await
            .map_err(|err| StripeError::Request(err.without_url().to_string()))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|err| StripeError::Request(err.without_url().to_string()))?;
        if status.is_success() {
            Ok(body)
        } else {
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("Stripe request failed")
                .to_string();
            Err(StripeError::Api(message))
        }
    }

    /// A connected account: `charges_enabled` says whether it can be paid.
    pub async fn account(&self, id: &str) -> Result<Value, StripeError> {
        self.get(&format!("/v1/accounts/{}", path_part(id)), &[])
            .await
    }

    /// Stripe's onboarding page for the account (business details, bank
    /// account). The link works once and for a few minutes.
    pub async fn create_account_link(
        &self,
        account: &str,
        refresh_url: &str,
        return_url: &str,
    ) -> Result<Value, StripeError> {
        self.post(
            "/v1/account_links",
            &[
                ("account", account.into()),
                ("refresh_url", refresh_url.into()),
                ("return_url", return_url.into()),
                ("type", "account_onboarding".into()),
            ],
        )
        .await
    }

    /// A Checkout page on the connected `account` for one payment. It expires
    /// in 24 hours. A card page says [`US_CARDS_ONLY`]; a bank transfer's
    /// takes only a bank account the customer signs in to (no numbers typed
    /// by hand, no micro-deposits).
    pub async fn create_payment_checkout_session(
        &self,
        account: &str,
        checkout: &PaymentCheckout<'_>,
    ) -> Result<Value, StripeError> {
        let metadata: Vec<(String, String)> = checkout
            .metadata
            .iter()
            .flat_map(|(key, value)| {
                [
                    (format!("metadata[{key}]"), value.clone()),
                    (
                        format!("payment_intent_data[metadata][{key}]"),
                        value.clone(),
                    ),
                ]
            })
            .collect();
        let mut form: Vec<(&str, String)> = vec![
            ("mode", "payment".into()),
            ("payment_method_types[0]", checkout.payment_method.into()),
            // Link would add its own ways to pay (a bank, Klarna) to a card
            // page, and the fee was worked out for the method chosen here.
            ("wallet_options[link][display]", "never".into()),
            // The price is in dollars: no offer to pay in the card's own
            // currency, which costs the customer a conversion markup.
            ("adaptive_pricing[enabled]", "false".into()),
            ("client_reference_id", checkout.payment_id.into()),
            ("metadata[granite_payment]", checkout.payment_id.into()),
            (
                "payment_intent_data[metadata][granite_payment]",
                checkout.payment_id.into(),
            ),
            ("line_items[0][quantity]", "1".into()),
            ("line_items[0][price_data][currency]", "usd".into()),
            (
                "line_items[0][price_data][unit_amount]",
                checkout.amount_cents.to_string(),
            ),
            (
                "line_items[0][price_data][product_data][name]",
                checkout.description.into(),
            ),
            ("success_url", checkout.success_url.into()),
            ("cancel_url", checkout.cancel_url.into()),
        ];
        if checkout.payment_method == "card" {
            // Only a notice next to Stripe's Pay button: a card issued
            // elsewhere is not refused, and costs the company 1.5% more.
            form.push(("custom_text[submit][message]", US_CARDS_ONLY.into()));
        }
        if checkout.payment_method == "us_bank_account" {
            // The customer signs in to their bank and the transfer starts at
            // once. Without this Stripe also lets them type the account's
            // numbers by hand, which it then confirms with two small
            // deposits over days: the page finishes with nothing debited.
            form.push((
                "payment_method_options[us_bank_account][verification_method]",
                "instant".into(),
            ));
        }
        if checkout.customer_fee_cents > 0 {
            form.extend([
                ("line_items[1][quantity]", "1".to_string()),
                ("line_items[1][price_data][currency]", "usd".to_string()),
                (
                    "line_items[1][price_data][unit_amount]",
                    checkout.customer_fee_cents.to_string(),
                ),
                (
                    "line_items[1][price_data][product_data][name]",
                    checkout.customer_fee_name.to_string(),
                ),
            ]);
        }
        form.extend(
            metadata
                .iter()
                .map(|(key, value)| (key.as_str(), value.clone())),
        );
        if checkout.application_fee_cents > 0 {
            form.push((
                "payment_intent_data[application_fee_amount]",
                checkout.application_fee_cents.to_string(),
            ));
        }
        // Stripe refuses a key sent again with other parameters, for 24
        // hours. What is sent can change between two tries (the job gets a
        // title, a newer quote), so the key carries a digest of it: the
        // same request twice still makes one session, a changed one makes
        // its own.
        let key = format!("{}-{}", checkout.idempotency_key, form_digest(&form));
        self.send_once(
            Some(account),
            Some(&key),
            reqwest::Method::POST,
            "/v1/checkout/sessions",
            &form,
        )
        .await
    }

    /// A Checkout session of the connected `account`: `status` is `open`,
    /// `complete` or `expired`.
    pub async fn checkout_session(&self, account: &str, id: &str) -> Result<Value, StripeError> {
        self.send_as(
            Some(account),
            reqwest::Method::GET,
            &format!("/v1/checkout/sessions/{}", path_part(id)),
            &[],
        )
        .await
    }

    /// Closes an open Checkout session, so nobody can pay on it any more.
    pub async fn expire_checkout_session(
        &self,
        account: &str,
        id: &str,
    ) -> Result<Value, StripeError> {
        self.send_as(
            Some(account),
            reqwest::Method::POST,
            &format!("/v1/checkout/sessions/{}/expire", path_part(id)),
            &[],
        )
        .await
    }

    /// A payment intent of the connected `account` (`last_payment_error`
    /// says why it failed).
    pub async fn payment_intent(&self, account: &str, id: &str) -> Result<Value, StripeError> {
        self.send_as(
            Some(account),
            reqwest::Method::GET,
            &format!("/v1/payment_intents/{}", path_part(id)),
            &[],
        )
        .await
    }
}

fn data(list: Value) -> Vec<Value> {
    match list {
        Value::Object(mut map) => match map.remove("data") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Ids go in URL paths; Stripe ids are `[A-Za-z0-9_]`, anything else is escaped.
fn path_part(id: &str) -> String {
    percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("No signatures found matching the expected signature for payload")]
    Mismatch,
    #[error("Unable to extract timestamp and signatures from header")]
    Malformed,
    #[error("Timestamp outside the tolerance zone")]
    TooOld,
}

/// Checks a `Stripe-Signature` header (`t=<unix>,v1=<hex hmac>,...`) against
/// the endpoint secret, like the SDK's `constructEvent`.
pub fn verify_signature(
    payload: &[u8],
    header: &str,
    secret: &str,
    now_unix: i64,
) -> Result<(), SignatureError> {
    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<&str> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.parse().ok(),
            Some(("v1", value)) => signatures.push(value),
            _ => {}
        }
    }
    let Some(timestamp) = timestamp else {
        return Err(SignatureError::Malformed);
    };
    if signatures.is_empty() {
        return Err(SignatureError::Malformed);
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| SignatureError::Mismatch)?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = hex(&mac.finalize().into_bytes());
    let matches = signatures
        .iter()
        .any(|sent| constant_time_eq(sent.as_bytes(), expected.as_bytes()));
    if !matches {
        return Err(SignatureError::Mismatch);
    }
    if now_unix - timestamp > SIGNATURE_TOLERANCE_SECS {
        return Err(SignatureError::TooOld);
    }
    Ok(())
}

/// The header Stripe would send for `payload` at `timestamp` (tests, tools).
pub fn signature_header(payload: &[u8], secret: &str, timestamp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    format!("t={timestamp},v1={}", hex(&mac.finalize().into_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Stripe objects -> snapshots

/// An id field: a string, or an expanded object's `id` (none when deleted).
pub fn stripe_id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(map) => {
            if map.get("deleted") == Some(&Value::Bool(true)) {
                return None;
            }
            map.get("id").and_then(Value::as_str).map(str::to_string)
        }
        _ => None,
    }
}

/// A positive whole number from a metadata string (`"12"`).
pub fn positive_int(value: &Value) -> Option<i32> {
    value
        .as_str()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|id| *id > 0)
}

fn unix_date(value: &Value) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(value.as_i64()?, 0).map(|date| date.naive_utc())
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn non_empty(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn subscription_snapshot(subscription: &Value) -> SubscriptionSnapshot {
    let item = &subscription["items"]["data"][0];
    let price = &item["price"];
    SubscriptionSnapshot {
        id: subscription["id"].as_str().unwrap_or_default().to_string(),
        customer_id: stripe_id(&subscription["customer"]).unwrap_or_default(),
        price_id: text(&price["id"]),
        item_id: text(&item["id"]),
        status: subscription["status"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        quantity: item["quantity"].as_i64(),
        period_start: unix_date(&item["current_period_start"]),
        period_end: unix_date(&item["current_period_end"]),
        payment_method_id: stripe_id(&subscription["default_payment_method"]),
        plan_name: non_empty(&price["nickname"]).or_else(|| non_empty(&item["plan"]["nickname"])),
        unit_amount_cents: price["unit_amount"].as_i64(),
        company_id: positive_int(&subscription["metadata"]["company_id"]),
        deleted: false,
    }
}

pub fn invoice_snapshot(invoice: &Value) -> InvoiceSnapshot {
    let details = &invoice["parent"]["subscription_details"];
    InvoiceSnapshot {
        id: invoice["id"].as_str().unwrap_or_default().to_string(),
        customer_id: stripe_id(&invoice["customer"]).unwrap_or_default(),
        subscription_id: stripe_id(&details["subscription"]),
        company_id: positive_int(&details["metadata"]["company_id"]),
        status: invoice["status"].as_str().unwrap_or("open").to_string(),
        amount_due_cents: invoice["amount_due"].as_i64().unwrap_or_default(),
        amount_paid_cents: invoice["amount_paid"].as_i64().unwrap_or_default(),
        currency: invoice["currency"].as_str().unwrap_or("usd").to_string(),
        hosted_invoice_url: text(&invoice["hosted_invoice_url"]),
        invoice_pdf: text(&invoice["invoice_pdf"]),
        paid_at: unix_date(&invoice["status_transitions"]["paid_at"]),
    }
}

/// The billing event in a webhook payload; `None` for events billing ignores.
pub fn normalize_event(event: &Value) -> Option<BillingEvent> {
    let id = event["id"].as_str()?.to_string();
    let object = &event["data"]["object"];
    let kind = object["object"].as_str().unwrap_or_default();
    match (event["type"].as_str()?, kind) {
        ("checkout.session.completed", "checkout.session") => {
            let company_id = positive_int(&object["metadata"]["company_id"])
                .or_else(|| positive_int(&object["client_reference_id"]))?;
            let customer_id = stripe_id(&object["customer"])?;
            Some(BillingEvent::CheckoutCompleted {
                id,
                company_id,
                customer_id,
                subscription_id: stripe_id(&object["subscription"]),
            })
        }
        (
            kind @ ("customer.subscription.created"
            | "customer.subscription.updated"
            | "customer.subscription.deleted"),
            "subscription",
        ) => Some(BillingEvent::Subscription {
            id,
            kind: match kind {
                "customer.subscription.created" => "customer.subscription.created",
                "customer.subscription.updated" => "customer.subscription.updated",
                _ => "customer.subscription.deleted",
            },
            subscription: subscription_snapshot(object),
        }),
        (kind @ ("invoice.paid" | "invoice.payment_failed"), "invoice") => {
            Some(BillingEvent::Invoice {
                id,
                paid: kind == "invoice.paid",
                invoice: invoice_snapshot(object),
            })
        }
        ("payment_method.updated", "payment_method") => Some(BillingEvent::PaymentMethodUpdated {
            id,
            customer_id: stripe_id(&object["customer"])?,
            payment_method_id: object["id"].as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// The event with Stripe's current state.
///
/// Webhooks can arrive late or out of order. A failed invoice that has since been paid becomes `invoice.paid`;
/// a voided one is dropped (`None`). Without a client the event is used as sent.
pub async fn latest_event(
    stripe: &Stripe,
    event: BillingEvent,
) -> Result<Option<BillingEvent>, StripeError> {
    if !stripe.is_enabled() {
        return Ok(Some(event));
    }
    match event {
        BillingEvent::Subscription {
            id,
            kind,
            subscription,
        } if kind != "customer.subscription.deleted" => {
            let live = stripe.subscription(&subscription.id).await?;
            Ok(Some(BillingEvent::Subscription {
                id,
                kind,
                subscription: subscription_snapshot(&live),
            }))
        }
        BillingEvent::Invoice {
            id,
            paid: false,
            invoice,
        } => {
            let live = stripe.invoice(&invoice.id).await?;
            match live["status"].as_str() {
                Some("void") => Ok(None),
                status => Ok(Some(BillingEvent::Invoice {
                    id,
                    paid: status == Some("paid"),
                    invoice: invoice_snapshot(&live),
                })),
            }
        }
        other => Ok(Some(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Without a connected account Stripe would act on the platform's own
    /// account: such a request is never sent.
    #[tokio::test]
    async fn a_request_for_a_connected_account_needs_one() {
        // Nothing listens here: a request that went out would fail otherwise.
        let stripe = Stripe::new(Some("sk_test_x"), "http://127.0.0.1:9");
        for account in ["", " ", "100", "cus_1"] {
            let refused = stripe.checkout_session(account, "cs_1").await;
            assert!(
                matches!(&refused, Err(StripeError::Request(message))
                    if message == "no connected Stripe account to act on"),
                "{account:?}: {refused:?}"
            );
        }
    }

    #[test]
    fn a_form_s_digest_changes_with_what_is_sent() {
        let form = |quote: &str| {
            vec![
                ("mode", "payment".to_string()),
                ("metadata[quote_id]", quote.to_string()),
            ]
        };
        assert_eq!(form_digest(&form("9100")), form_digest(&form("9100")));
        assert_ne!(form_digest(&form("9100")), form_digest(&form("9101")));
        // Where one value ends and the next begins counts.
        assert_ne!(
            form_digest(&[("a", "bc".to_string())]),
            form_digest(&[("ab", "c".to_string())])
        );
        assert_eq!(form_digest(&form("9100")).len(), 16);
    }

    #[test]
    fn signatures_are_checked_like_the_sdk() {
        let payload = br#"{"id":"evt_test"}"#;
        let header = signature_header(payload, "whsec_test_secret", 1_000);
        assert_eq!(
            verify_signature(payload, &header, "whsec_test_secret", 1_100),
            Ok(())
        );
        assert_eq!(
            verify_signature(payload, &header, "whsec_other", 1_100),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(b"{}", &header, "whsec_test_secret", 1_100),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(payload, &header, "whsec_test_secret", 1_301),
            Err(SignatureError::TooOld)
        );
        assert_eq!(
            verify_signature(payload, "v1=abc", "whsec_test_secret", 1_000),
            Err(SignatureError::Malformed)
        );
        // Several v1 signatures (secret rotation): any may match.
        let rotated = format!("t=1000,v1=00,{}", header.split_once(',').unwrap().1);
        assert_eq!(
            verify_signature(payload, &rotated, "whsec_test_secret", 1_000),
            Ok(())
        );
    }

    #[test]
    fn subscriptions_read_their_first_item() {
        let snapshot = subscription_snapshot(&json!({
            "id": "sub_1", "object": "subscription", "status": "active",
            "customer": { "id": "cus_1", "object": "customer" },
            "default_payment_method": "pm_1",
            "metadata": { "company_id": "100" },
            "items": { "data": [{
                "id": "si_1", "quantity": 3,
                "current_period_start": 1_788_220_800, "current_period_end": 1_790_812_800,
                "price": { "id": "price_monthly", "nickname": "", "unit_amount": 4900 },
                "plan": { "nickname": "Granite Manager" },
            }] },
        }));
        assert_eq!(snapshot.customer_id, "cus_1");
        assert_eq!(snapshot.plan_name.as_deref(), Some("Granite Manager"));
        assert_eq!(snapshot.quantity, Some(3));
        assert_eq!(snapshot.unit_amount_cents, Some(4900));
        assert_eq!(snapshot.company_id, Some(100));
        assert_eq!(
            snapshot.period_start.unwrap().to_string(),
            "2026-09-01 00:00:00"
        );
    }

    #[test]
    fn events_billing_ignores_are_none() {
        assert_eq!(
            normalize_event(
                &json!({ "id": "evt", "type": "charge.succeeded", "data": { "object": { "object": "charge" } } })
            ),
            None
        );
        // A checkout without a company cannot be placed.
        assert_eq!(
            normalize_event(&json!({ "id": "evt", "type": "checkout.session.completed",
                "data": { "object": { "object": "checkout.session", "customer": "cus_1", "metadata": {} } } })),
            None
        );
        assert_eq!(
            normalize_event(&json!({ "id": "evt", "type": "checkout.session.completed",
                "data": { "object": { "object": "checkout.session", "customer": "cus_1",
                    "client_reference_id": "7", "subscription": "sub_1" } } })),
            Some(BillingEvent::CheckoutCompleted {
                id: "evt".into(),
                company_id: 7,
                customer_id: "cus_1".into(),
                subscription_id: Some("sub_1".into()),
            })
        );
    }

    #[test]
    fn metadata_ids_are_positive_whole_numbers() {
        assert_eq!(positive_int(&json!("12")), Some(12));
        assert_eq!(positive_int(&json!(" 12 ")), Some(12));
        assert_eq!(positive_int(&json!("0")), None);
        assert_eq!(positive_int(&json!("1.5")), None);
        assert_eq!(positive_int(&json!("abc")), None);
        assert_eq!(positive_int(&json!(12)), None);
    }
}
