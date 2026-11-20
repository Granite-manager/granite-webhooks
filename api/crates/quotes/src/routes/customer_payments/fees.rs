//! Fees: what a payment costs, who pays it and how the sums are written.

use super::{ACH_RATE_BPS, CARD_MIN_FEE_CENTS, CARD_RATE_BPS};
use crate::openapi::ToSchema;
use serde::{Deserialize, Serialize};

/// How a customer pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PaymentMethod {
    /// A credit or debit card: paid at once.
    Card,
    /// A US bank transfer: about 4 business days.
    Ach,
}

impl PaymentMethod {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Ach => "ach",
        }
    }

    pub(super) fn parse(value: &str) -> Self {
        if value == "ach" {
            Self::Ach
        } else {
            Self::Card
        }
    }

    /// How a message names it.
    pub(super) const fn words(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Ach => "ACH",
        }
    }

    pub(super) const fn rate_bps(self) -> i32 {
        match self {
            Self::Card => CARD_RATE_BPS,
            Self::Ach => ACH_RATE_BPS,
        }
    }

    /// The least its fee can be, in cents.
    pub(super) const fn min_fee(self) -> i64 {
        match self {
            Self::Card => CARD_MIN_FEE_CENTS,
            Self::Ach => 0,
        }
    }

    /// Stripe's `payment_method_types` value.
    pub(super) const fn stripe_type(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Ach => "us_bank_account",
        }
    }

    /// Stripe's usual fee on a charge of `charged` cents: 2.9% + 30¢ on a
    /// card, 0.8% capped at $5 on ACH.
    const fn stripe_fee(self, charged: i64) -> i64 {
        match self {
            Self::Card => bps_of(charged, 290) + 30,
            Self::Ach => {
                let fee = bps_of(charged, 80);
                if fee > 500 { 500 } else { fee }
            }
        }
    }
}

/// `bps` basis points of `cents`, rounded half up like JavaScript's
/// `Math.round` (the CRM does the same sums for its previews).
const fn bps_of(cents: i64, bps: i64) -> i64 {
    (cents * bps + 5_000) / 10_000
}

/// A payment's fee and who pays it, in cents.
#[derive(Debug, PartialEq, Eq)]
pub struct FeeSplit {
    /// The whole fee the company is quoted.
    pub fee: i64,
    /// The part added to what the customer pays.
    pub customer_fee: i64,
    /// The part taken from what the company receives.
    pub company_fee: i64,
    /// What the platform keeps: the fee less Stripe's usual fee, never below
    /// zero (Stripe charges its own fee to the company).
    pub application_fee: i64,
}

/// Splits the fee on `amount` cents. The customer's part is rounded first
/// and the company pays the rest, so the two always add up to the fee.
pub fn split_fee(method: PaymentMethod, amount: i64, customer_share_bps: i32) -> FeeSplit {
    let rate = method.rate_bps();
    let share = customer_share_bps.clamp(0, rate);
    let fee = bps_of(amount, i64::from(rate)).max(method.min_fee());
    let customer_fee = bps_of(amount, i64::from(share)).min(fee);
    let charged = amount + customer_fee;
    FeeSplit {
        fee,
        customer_fee,
        company_fee: fee - customer_fee,
        application_fee: (fee - method.stripe_fee(charged)).max(0),
    }
}

/// `500` → `$5`, `750` → `$7.50`, `100000` → `$1,000`.
pub(super) fn dollars(cents: i64) -> String {
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match cents % 100 {
        0 => format!("${grouped}"),
        rest => format!("${grouped}.{rest:02}"),
    }
}

/// `50` → `0.5%`, `100` → `1%`, `5` → `0.05%`.
fn percent(bps: i32) -> String {
    let (whole, rest) = (bps / 100, bps % 100);
    if rest == 0 {
        format!("{whole}%")
    } else {
        let rest = format!("{rest:02}");
        format!("{whole}.{}%", rest.trim_end_matches('0'))
    }
}

/// The name of the fee's line on Stripe's page: `Processing fee (0.5%)`.
pub(super) fn fee_line_name(customer_fee_bps: i32) -> String {
    if customer_fee_bps > 0 {
        format!("Processing fee ({})", percent(customer_fee_bps))
    } else {
        "Processing fee".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fee_is_split_like_the_crm_previews_it() {
        // $1,000 by card, the company pays all: 3% = $30; Stripe takes about
        // $29.30 of it, so $0.70 is left for the platform.
        assert_eq!(
            split_fee(PaymentMethod::Card, 100_000, 0),
            FeeSplit {
                fee: 3_000,
                customer_fee: 0,
                company_fee: 3_000,
                application_fee: 70,
            }
        );
        // $1,000 by ACH, the customer pays 0.4% of the 1%: Stripe's 0.8% of
        // $1,004 is capped at $5.
        assert_eq!(
            split_fee(PaymentMethod::Ach, 100_000, 40),
            FeeSplit {
                fee: 1_000,
                customer_fee: 400,
                company_fee: 600,
                application_fee: 500,
            }
        );
        // Half cents round up, and the parts still add up.
        let split = split_fee(PaymentMethod::Ach, 150, 50);
        assert_eq!(
            (split.fee, split.customer_fee, split.company_fee),
            (2, 1, 1)
        );
    }

    #[test]
    fn the_platform_never_pays_to_take_a_payment() {
        // $100 by card: 3% would be $3.00 and Stripe takes $3.20, so the
        // fee is the $9 minimum and $5.80 is left.
        let small = split_fee(PaymentMethod::Card, 10_000, 0);
        assert_eq!(
            (small.fee, small.company_fee, small.application_fee),
            (900, 900, 580)
        );
        // $300 is where 3% reaches the minimum.
        assert_eq!(split_fee(PaymentMethod::Card, 30_000, 0).fee, 900);
        assert_eq!(split_fee(PaymentMethod::Card, 30_100, 0).fee, 903);
        // The smallest card payment still holds its fee.
        let least = split_fee(PaymentMethod::Card, 1_000, 0);
        assert_eq!((least.fee, least.application_fee), (900, 841));
        // ACH has no minimum.
        assert_eq!(split_fee(PaymentMethod::Ach, 10_000, 0).fee, 100);
        // A share above the rate is the whole fee, never more.
        let split = split_fee(PaymentMethod::Ach, 100_000, 900);
        assert_eq!((split.customer_fee, split.company_fee), (1_000, 0));
    }

    #[test]
    fn money_reads_like_the_crm_writes_it() {
        assert_eq!(dollars(500), "$5");
        assert_eq!(dollars(750), "$7.50");
        assert_eq!(dollars(100_000), "$1,000");
        assert_eq!(dollars(123_456_705), "$1,234,567.05");
        assert_eq!(dollars(99), "$0.99");
    }

    #[test]
    fn the_fee_line_names_the_customer_s_share() {
        assert_eq!(fee_line_name(50), "Processing fee (0.5%)");
        assert_eq!(fee_line_name(100), "Processing fee (1%)");
        assert_eq!(fee_line_name(5), "Processing fee (0.05%)");
        assert_eq!(fee_line_name(0), "Processing fee");
    }
}
