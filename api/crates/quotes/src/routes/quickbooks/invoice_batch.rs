//! Many customers' `QuickBooks` invoices in as few requests as it takes (the
//! commissions sync): one invoice query per [`CHUNK`] linked customers
//! instead of one per customer, and one read of the `QuickBooks` customer
//! list to match every customer not linked yet instead of two searches each.

use super::invoice::{InvoiceSummary, summary, text};
use super::{QboCustomer, invoices_for, pick_by_name, qbo_customer, resolve_qbo_id};
use crate::error::ApiResult;
use crate::qbo::{Access, QboError, escape, valid_id};
use crate::state::AppState;
use lambda_http::tracing;
use serde_json::Value;
use std::collections::HashMap;

/// `QuickBooks` customers per invoice query: keeps the query's URL short.
const CHUNK: usize = 60;
/// The most rows `QuickBooks` returns for one query.
const PAGE: usize = 1000;
/// Up to this many customers not linked yet are searched one by one (two
/// requests each at most); more are matched against the whole customer list.
const SEARCH_ONE_BY_ONE: usize = 3;

/// What [`invoices_of_customers`] found.
pub struct CustomersInvoices {
    /// `ok`, `not_connected` (nothing was read) or `error` (some lookups failed).
    pub status: &'static str,
    /// CRM customer id → its invoices, newest first. A customer with no
    /// `QuickBooks` customer, or whose lookup failed, is not in it.
    pub invoices: HashMap<i32, Vec<InvoiceSummary>>,
    /// The requests sent to `QuickBooks`.
    pub requests: usize,
}

#[derive(sqlx::FromRow)]
struct Known {
    id: i32,
    name: Option<String>,
    qbo_id: Option<String>,
    email: Option<String>,
}

/// The invoice query for some `QuickBooks` customers, from row `start` (1-based).
fn invoices_query(qbo_ids: &[&str], start: usize) -> String {
    let ids: Vec<String> = qbo_ids
        .iter()
        .map(|id| format!("'{}'", escape(id.trim())))
        .collect();
    format!(
        "select Id, DocNumber, TxnDate, DueDate, TotalAmt, Balance, CurrencyRef, ShipAddr, MetaData, CustomerRef from Invoice where CustomerRef in ({}) STARTPOSITION {start} MAXRESULTS {PAGE}",
        ids.join(", ")
    )
}

/// Invoice rows by their `QuickBooks` customer, each list newest first.
fn by_customer(rows: &[Value]) -> HashMap<String, Vec<InvoiceSummary>> {
    let mut found: HashMap<String, Vec<InvoiceSummary>> = HashMap::new();
    for row in rows {
        if let (Some(customer), Some(invoice)) = (text(&row["CustomerRef"]["value"]), summary(row))
        {
            found
                .entry(customer.trim().to_string())
                .or_default()
                .push(invoice);
        }
    }
    for list in found.values_mut() {
        list.sort_by(|a, b| b.txn_date.cmp(&a.txn_date));
    }
    found
}

/// The `QuickBooks` customer a CRM customer is, picked from the whole list as
/// [`resolve_qbo_id`] picks from its two searches: the only one with their
/// email, else by name among those whose name contains theirs.
fn match_in_list(all: &[QboCustomer], name: &str, email: &str) -> Option<String> {
    let key = |value: &str| value.trim().to_lowercase();
    let (name, email) = (key(&name.replace('%', "")), key(email));
    if !email.is_empty() {
        let same: Vec<&QboCustomer> = all
            .iter()
            .filter(|c| c.email.as_deref().is_some_and(|e| key(e) == email))
            .collect();
        if let [only] = same.as_slice() {
            return Some(only.id.trim().to_string());
        }
    }
    if name.is_empty() {
        return None;
    }
    let named: Vec<QboCustomer> = all
        .iter()
        .filter(|c| key(&c.display_name).contains(&name))
        .map(|c| QboCustomer {
            id: c.id.clone(),
            display_name: c.display_name.clone(),
            email: c.email.clone(),
        })
        .collect();
    pick_by_name(&named, &name, &email)
}

/// Every row of a query, a page at a time. `query` gets the 1-based start row.
async fn all_rows(
    state: &AppState,
    access: &Access,
    entity: &str,
    requests: &mut usize,
    query: impl Fn(usize) -> String,
) -> Result<Vec<Value>, QboError> {
    let mut rows = Vec::new();
    loop {
        *requests += 1;
        let page = state
            .qbo
            .query(access, &query(rows.len() + 1), entity)
            .await?;
        let last = page.len() < PAGE;
        rows.extend(page);
        if last {
            return Ok(rows);
        }
    }
}

/// The `QuickBooks` invoices of the company's `customer_ids`, for the
/// commissions sync. Customers not linked to `QuickBooks` yet are matched (by
/// email, then name) and linked first, as [`super::invoices_of_customer`]
/// does for one.
#[allow(clippy::too_many_lines)]
pub async fn invoices_of_customers(
    state: &AppState,
    company_id: i32,
    customer_ids: &[i32],
) -> ApiResult<CustomersInvoices> {
    let mut out = CustomersInvoices {
        status: "ok",
        invoices: HashMap::new(),
        requests: 0,
    };
    if customer_ids.is_empty() {
        return Ok(out);
    }
    let mut select = sqlx::QueryBuilder::new(
        "SELECT c.id, c.name, NULLIF(TRIM(CAST(c.qbo_id AS CHAR)), '') AS qbo_id, ce.email
           FROM customers c
           LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id
          WHERE c.deleted_at IS NULL AND c.company_id = ",
    );
    select.push_bind(company_id).push(" AND c.id IN (");
    let mut ids = select.separated(", ");
    for id in customer_ids {
        ids.push_bind(*id);
    }
    select.push(")");
    let mut customers: Vec<Known> = select.build_query_as().fetch_all(&state.pool).await?;

    let access = match state.qbo.access(&state.pool, company_id).await {
        Ok(access) => access,
        Err(err) if err.is_not_connected() => {
            out.status = "not_connected";
            return Ok(out);
        }
        Err(QboError::Database(err)) => return Err(err.into()),
        Err(err) => {
            tracing::error!(error = %err, "QuickBooks access failed");
            out.status = "error";
            return Ok(out);
        }
    };

    // Customers not linked yet: find their QuickBooks customer and link it.
    let unlinked = customers.iter().filter(|c| c.qbo_id.is_none()).count();
    let list: Option<Vec<QboCustomer>> = if unlinked > SEARCH_ONE_BY_ONE {
        let read = all_rows(state, &access, "Customer", &mut out.requests, |start| {
            format!(
                "select Id, DisplayName, PrimaryEmailAddr from Customer STARTPOSITION {start} MAXRESULTS {PAGE}"
            )
        })
        .await;
        match read {
            Ok(rows) => Some(rows.iter().filter_map(qbo_customer).collect()),
            Err(err) => {
                tracing::error!(error = %err, "QuickBooks customer list failed");
                out.status = "error";
                None
            }
        }
    } else {
        None
    };
    for customer in customers.iter_mut().filter(|c| c.qbo_id.is_none()) {
        let name = customer.name.as_deref().unwrap_or("");
        let email = customer.email.as_deref().unwrap_or("");
        let found = if let Some(all) = &list {
            match_in_list(all, name, email)
        } else if unlinked > SEARCH_ONE_BY_ONE {
            // The list could not be read.
            None
        } else {
            out.requests += usize::from(!email.trim().is_empty()) + 1;
            let row = super::CustomerRow {
                name: customer.name.clone(),
                email: customer.email.clone(),
                ..Default::default()
            };
            match resolve_qbo_id(state, &access, &row).await {
                Ok(found) => found,
                Err(QboError::Database(err)) => return Err(err.into()),
                Err(err) => {
                    tracing::error!(error = %err, customer_id = customer.id, "QuickBooks customer search failed");
                    out.status = "error";
                    None
                }
            }
        };
        if let Some(id) = found {
            sqlx::query(
                "UPDATE customers SET qbo_id = ? WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
            )
            .bind(&id)
            .bind(customer.id)
            .bind(company_id)
            .execute(&state.pool)
            .await?;
            customer.qbo_id = Some(id);
        }
    }

    // Their invoices, a chunk of QuickBooks customers per query.
    let mut qbo_ids: Vec<&str> = customers
        .iter()
        .filter_map(|c| c.qbo_id.as_deref())
        .map(str::trim)
        .filter(|id| valid_id(id))
        .collect();
    qbo_ids.sort_unstable();
    qbo_ids.dedup();
    let mut found: HashMap<String, Vec<InvoiceSummary>> = HashMap::new();
    for chunk in qbo_ids.chunks(CHUNK) {
        let read = all_rows(state, &access, "Invoice", &mut out.requests, |start| {
            invoices_query(chunk, start)
        })
        .await;
        match read {
            Ok(rows) => {
                // One with no invoices is still known: an empty list.
                for id in chunk {
                    found.entry((*id).to_string()).or_default();
                }
                for (id, invoices) in by_customer(&rows) {
                    found.insert(id, invoices);
                }
            }
            Err(err) => {
                // QuickBooks refused the joint query: ask customer by customer.
                tracing::warn!(error = %err, "QuickBooks joint invoice query failed; asking one by one");
                for id in chunk {
                    out.requests += 1;
                    match invoices_for(state, &access, id).await {
                        Ok(invoices) => {
                            found.insert((*id).to_string(), invoices);
                        }
                        Err(err) => {
                            tracing::error!(error = %err, qbo_id = id, "QuickBooks invoices failed");
                            out.status = "error";
                        }
                    }
                }
            }
        }
    }
    for customer in &customers {
        if let Some(invoices) = customer
            .qbo_id
            .as_deref()
            .and_then(|id| found.get(id.trim()))
        {
            out.invoices.insert(customer.id, invoices.clone());
        }
    }
    tracing::info!(
        company_id,
        customers = customers.len(),
        requests = out.requests,
        "QuickBooks invoices read for the commissions sync"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn customer(id: &str, name: &str, email: Option<&str>) -> QboCustomer {
        QboCustomer {
            id: id.into(),
            display_name: name.into(),
            email: email.map(Into::into),
        }
    }

    #[test]
    fn one_query_asks_for_a_chunk_of_customers() {
        assert_eq!(
            invoices_query(&["42", " 7 "], 1001),
            "select Id, DocNumber, TxnDate, DueDate, TotalAmt, Balance, CurrencyRef, ShipAddr, MetaData, CustomerRef from Invoice where CustomerRef in ('42', '7') STARTPOSITION 1001 MAXRESULTS 1000"
        );
    }

    #[test]
    fn invoices_group_by_customer_newest_first() {
        let rows = [
            json!({ "Id": "1", "TxnDate": "2026-01-05", "CustomerRef": { "value": "42" } }),
            json!({ "Id": "2", "TxnDate": "2026-03-01", "CustomerRef": { "value": "42" } }),
            json!({ "Id": "3", "TxnDate": "2026-02-01", "CustomerRef": { "value": "7" } }),
            json!({ "Id": "4", "TxnDate": "2026-02-01" }),
        ];
        let found = by_customer(&rows);
        let ids = |customer: &str| -> Vec<&str> {
            found[customer].iter().map(|i| i.id.as_str()).collect()
        };
        assert_eq!(found.len(), 2);
        assert_eq!(ids("42"), ["2", "1"]);
        assert_eq!(ids("7"), ["3"]);
    }

    #[test]
    fn a_customer_is_matched_by_their_only_email_then_by_name() {
        let all = [
            customer("1", "Alice Lead", Some("Alice@Example.com")),
            customer("2", "Bob Stone", Some("bob@example.com")),
            customer("3", "Bob Stone Jr", Some("jr@example.com")),
            customer("4", "Twin", Some("twin@example.com")),
            customer("5", "Twin", Some("twin@example.com")),
        ];
        // The only one with the email, whatever its name.
        assert_eq!(
            match_in_list(&all, "Somebody", "alice@example.com").as_deref(),
            Some("1")
        );
        // No email match: the exact name among those containing it.
        assert_eq!(
            match_in_list(&all, "bob stone", "none@example.com").as_deref(),
            Some("2")
        );
        // Two with the same email and name: nobody.
        assert_eq!(match_in_list(&all, "Twin", "twin@example.com"), None);
        // No name and no email match.
        assert_eq!(match_in_list(&all, "", "none@example.com"), None);
        assert_eq!(match_in_list(&all, "Zed", ""), None);
    }
}
