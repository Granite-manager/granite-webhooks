//! What a company's `QuickBooks` invoices usually have besides their lines.
//!
//! Whether they carry a shipping address, when they are due and under which
//! terms, and whether their priced lines carry a service date. A new invoice
//! made from a quote gets the same, so nobody has to add it in `QuickBooks`.
//!
//! Counted from the newest invoices (`qbo_invoice_scans`), never guessed: a
//! habit needs at least [`MIN_SEEN`] invoices and 2 of 3 of them.

use super::invoice::{Address, rows, text};
use crate::openapi::ToSchema;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Fewest invoices (or priced lines) a habit is read from.
const MIN_SEEN: usize = 3;
/// A due date further out than this is a typo, not terms.
const MAX_DUE_DAYS: i64 = 366;

/// When one invoice is due, and under which terms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Due {
    /// Days from the invoice's date to its due date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<i64>,
    /// Its `QuickBooks` terms (`SalesTermRef`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
}

/// What the scanned invoices did, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Habits {
    /// Invoices read.
    pub invoices: usize,
    /// Those with a shipping address.
    pub ship: usize,
    /// Their priced lines, and those of them with a service date.
    pub priced_lines: usize,
    pub service_dated: usize,
    /// Each invoice's due date and terms, newest first.
    pub due: Vec<Due>,
}

/// When the company's invoices are usually due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UsualDue {
    /// Days after the invoice's date.
    pub days: i64,
    /// The terms such invoices carry, when most carry the same.
    pub term: Option<String>,
}

fn has_address(value: &Value) -> bool {
    ["Line1", "Line2", "Line3", "City", "PostalCode"]
        .iter()
        .any(|key| text(&value[*key]).is_some())
}

fn date(value: &Value) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.as_str()?.trim(), "%Y-%m-%d").ok()
}

/// At least [`MIN_SEEN`] and 2 of 3.
const fn usual(count: usize, of: usize) -> bool {
    count >= MIN_SEEN && count * 3 >= of * 2
}

impl Habits {
    /// Counted from `QuickBooks`' own invoice objects, newest first.
    pub fn read(invoices: &[Value]) -> Self {
        let mut habits = Self {
            invoices: invoices.len(),
            ..Self::default()
        };
        for invoice in invoices {
            if has_address(&invoice["ShipAddr"]) {
                habits.ship += 1;
            }
            for line in rows(&invoice["Line"]) {
                if line["DetailType"] == "SalesItemLineDetail" {
                    habits.priced_lines += 1;
                    if text(&line["SalesItemLineDetail"]["ServiceDate"]).is_some() {
                        habits.service_dated += 1;
                    }
                }
            }
            let days = date(&invoice["TxnDate"])
                .zip(date(&invoice["DueDate"]))
                .map(|(made, due)| (due - made).num_days())
                .filter(|days| (0..=MAX_DUE_DAYS).contains(days));
            habits.due.push(Due {
                days,
                term: text(&invoice["SalesTermRef"]["value"]),
            });
        }
        habits
    }

    /// The company's invoices carry a shipping address. Before enough
    /// invoices are read it is kept, as invoices always had it.
    pub const fn ships(&self) -> bool {
        self.invoices < MIN_SEEN || self.ship * 2 >= self.invoices
    }

    /// Priced lines usually carry a service date.
    pub const fn service_dates(&self) -> bool {
        usual(self.service_dated, self.priced_lines)
    }

    /// The days until due most invoices share, with the terms most of those
    /// carry.
    pub fn usual_due(&self) -> Option<UsualDue> {
        let known: Vec<&Due> = self.due.iter().filter(|due| due.days.is_some()).collect();
        let days = most(known.iter().filter_map(|due| due.days), known.len())?;
        let same: Vec<&&Due> = known.iter().filter(|due| due.days == Some(days)).collect();
        let term = most(same.iter().filter_map(|due| due.term.clone()), same.len());
        Some(UsualDue { days, term })
    }
}

/// The value at least [`MIN_SEEN`] and 2 of 3 of `of` share.
fn most<T: PartialEq>(values: impl Iterator<Item = T>, of: usize) -> Option<T> {
    let mut counts: Vec<(T, usize)> = Vec::new();
    for value in values {
        match counts.iter_mut().find(|(known, _)| *known == value) {
            Some((_, count)) => *count += 1,
            None => counts.push((value, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .filter(|(_, count)| usual(*count, of))
        .map(|(value, _)| value)
}

/// The addresses a new invoice opens with.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct DraftAddresses {
    /// The billing address: the customer's name, then their address, as
    /// `QuickBooks` writes it. Null leaves it to `QuickBooks`, which fills in
    /// the billing address its customer already has.
    pub bill: Option<Address>,
    /// The shipping (job) address. Null when the company's invoices carry
    /// none and the job is at the billing address.
    pub ship: Option<Address>,
}

fn squashed(text: Option<&str>) -> String {
    text.unwrap_or_default()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Two addresses on the same street line.
fn same_street(a: &Address, b: &Address) -> bool {
    let (a, b) = (squashed(a.line1.as_deref()), squashed(b.line1.as_deref()));
    !a.is_empty() && a == b
}

/// A `QuickBooks` billing address names this street on one of its lines.
fn names_street(billing: &Value, street: &Address) -> bool {
    let wanted = squashed(street.line1.as_deref());
    !wanted.is_empty()
        && ["Line1", "Line2", "Line3", "Line4", "Line5"]
            .iter()
            .any(|key| squashed(billing[*key].as_str()).contains(&wanted))
}

/// An address under the customer's name, as `QuickBooks` writes a billing
/// address (the name prints in the invoice's "Bill to").
fn under_name(name: &str, address: &Address) -> Address {
    let name = name.trim();
    if name.is_empty() {
        return address.clone();
    }
    Address {
        line1: Some(name.to_string()),
        line2: address.line1.clone(),
        line3: address.line2.clone(),
        city: address.city.clone(),
        country_sub_division_code: address.country_sub_division_code.clone(),
        postal_code: address.postal_code.clone(),
    }
}

/// Who an invoice is billed to and where the job is.
pub(super) struct Places<'a> {
    /// The customer's display name.
    pub name: &'a str,
    /// A builder or other business: billed at its office, never at the job.
    pub business: bool,
    /// The customer's own address in the CRM.
    pub own: Option<&'a Address>,
    /// The quote's job address.
    pub job: Option<&'a Address>,
    /// The billing address the customer already has in `QuickBooks`.
    pub qbo_billing: Option<&'a Value>,
}

impl Habits {
    /// The addresses a new invoice for `places` opens with; none when the
    /// CRM knows no address.
    ///
    /// The billing address is the customer's (a person without one is billed
    /// at the job); a customer with a billing address in `QuickBooks` keeps
    /// that one. The job goes in the shipping address when the company's
    /// invoices carry one, or when it is not at the billing address.
    pub fn addresses(&self, places: &Places) -> Option<DraftAddresses> {
        // A business is billed at its office and never has its job there.
        let site = places
            .job
            .or_else(|| places.own.filter(|_| !places.business));
        let billed_at = places
            .own
            .or_else(|| places.job.filter(|_| !places.business));
        if site.is_none() && billed_at.is_none() {
            return None;
        }
        let qbo_billing = places.qbo_billing.filter(|billing| has_address(billing));
        let at_billing = site.is_some_and(|site| {
            qbo_billing.map_or_else(
                || billed_at.is_some_and(|billed| same_street(billed, site)),
                |billing| names_street(billing, site),
            )
        });
        Some(DraftAddresses {
            bill: match qbo_billing {
                Some(_) => None,
                None => billed_at.map(|address| under_name(places.name, address)),
            },
            ship: site.filter(|_| self.ships() || !at_billing).cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn invoice(ship: bool, made: &str, due: &str, term: Option<&str>, dated: bool) -> Value {
        let mut invoice = json!({
            "TxnDate": made, "DueDate": due,
            "BillAddr": { "Line1": "Alice Lead", "Line2": "1 Main St" },
            "Line": [
                { "DetailType": "SalesItemLineDetail", "SalesItemLineDetail": {} },
                { "DetailType": "DescriptionOnly", "Description": "Flat edge" },
                { "DetailType": "SubTotalLineDetail" },
            ],
        });
        if ship {
            invoice["ShipAddr"] = json!({ "Line1": "1 Main St", "City": "Carmel" });
        }
        if let Some(term) = term {
            invoice["SalesTermRef"] = json!({ "value": term });
        }
        if dated {
            invoice["Line"][0]["SalesItemLineDetail"]["ServiceDate"] = json!(made);
        }
        invoice
    }

    #[test]
    fn habits_are_what_two_of_three_invoices_do() {
        let invoices = [
            invoice(false, "2026-10-01", "2026-10-31", Some("3"), true),
            invoice(false, "2026-10-28", "2026-11-27", Some("3"), true),
            invoice(true, "2026-09-01", "2026-09-01", None, false),
            invoice(false, "2026-08-01", "2026-08-31", Some("3"), true),
        ];
        let habits = Habits::read(&invoices);
        assert_eq!((habits.invoices, habits.ship), (4, 1));
        assert!(!habits.ships());
        assert!(habits.service_dates());
        assert_eq!(
            habits.usual_due(),
            Some(UsualDue {
                days: 30,
                term: Some("3".into())
            })
        );
        // Kept and read back as it is stored.
        let stored: Habits =
            serde_json::from_str(&serde_json::to_string(&habits).unwrap()).unwrap();
        assert_eq!(stored, habits);

        // No habit from two invoices, mixed terms, or a few dated lines.
        let few = Habits::read(&invoices[..2]);
        assert_eq!(few.usual_due(), None);
        assert!(!few.service_dates());
        assert!(few.ships());
        let mixed = Habits::read(&[
            invoice(true, "2026-10-01", "2026-10-31", Some("3"), false),
            invoice(true, "2026-10-01", "2026-10-01", Some("1"), true),
            invoice(false, "2026-10-01", "2026-10-15", None, false),
            invoice(true, "2026-10-01", "bad", None, false),
        ]);
        assert_eq!(mixed.usual_due(), None);
        assert!(!mixed.service_dates());
        assert!(mixed.ships());
        // Due on receipt without terms.
        let receipt = Habits::read(&[
            invoice(false, "2026-10-01", "2026-10-01", None, false),
            invoice(false, "2026-10-02", "2026-10-02", Some("1"), false),
            invoice(false, "2026-10-03", "2026-10-03", None, false),
        ]);
        assert_eq!(
            receipt.usual_due(),
            Some(UsualDue {
                days: 0,
                term: None
            })
        );
    }

    fn at(street: &str) -> Address {
        Address {
            line1: Some(street.into()),
            city: Some("Carmel".into()),
            country_sub_division_code: Some("IN".into()),
            postal_code: Some("46032".into()),
            ..Address::default()
        }
    }

    fn lines(address: Option<&Address>) -> Vec<String> {
        address
            .map(|address| {
                [&address.line1, &address.line2, &address.city]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_address_goes_where_the_companys_invoices_put_it() {
        let (home, job) = (at("1 Main St"), at("9 Oak Ave"));
        let person = |own: Option<&'static Address>, job: Option<&'static Address>| Places {
            name: "Alice Lead",
            business: false,
            own,
            job,
            qbo_billing: None,
        };
        let home: &'static Address = Box::leak(Box::new(home));
        let job: &'static Address = Box::leak(Box::new(job));
        let bills = Habits {
            invoices: 10,
            ship: 1,
            ..Habits::default()
        };
        let ships = Habits {
            invoices: 10,
            ship: 9,
            ..Habits::default()
        };

        // A company that bills only: the job at the customer's home goes in
        // the billing address, under the customer's name.
        let made = bills.addresses(&person(None, Some(home))).unwrap();
        assert_eq!(
            lines(made.bill.as_ref()),
            ["Alice Lead", "1 Main St", "Carmel"]
        );
        assert!(made.ship.is_none());
        // A job somewhere else is still said, in the shipping address.
        let made = bills.addresses(&person(Some(home), Some(job))).unwrap();
        assert_eq!(lines(made.bill.as_ref())[1], "1 Main St");
        assert_eq!(lines(made.ship.as_ref()), ["9 Oak Ave", "Carmel"]);
        // A company whose invoices carry a shipping address gets both.
        let made = ships.addresses(&person(None, Some(home))).unwrap();
        assert_eq!(lines(made.bill.as_ref())[1], "1 Main St");
        assert_eq!(lines(made.ship.as_ref())[0], "1 Main St");
        // A builder is billed at its office or, unknown, not by the CRM.
        let builder = Places {
            name: "Cedarbrook Properties",
            business: true,
            own: None,
            job: Some(job),
            qbo_billing: None,
        };
        let made = bills.addresses(&builder).unwrap();
        assert!(made.bill.is_none());
        assert_eq!(lines(made.ship.as_ref())[0], "9 Oak Ave");
        // The billing address QuickBooks has stays QuickBooks' own.
        let kept = json!({ "Line1": "Alice Lead", "Line2": "1 Main St.", "Line3": "Carmel, IN" });
        let linked = Places {
            qbo_billing: Some(&kept),
            ..person(None, Some(home))
        };
        let made = bills.addresses(&linked).unwrap();
        assert!(made.bill.is_none() && made.ship.is_none());
        let elsewhere = Places {
            qbo_billing: Some(&kept),
            ..person(None, Some(job))
        };
        assert_eq!(
            lines(bills.addresses(&elsewhere).unwrap().ship.as_ref())[0],
            "9 Oak Ave"
        );
        // Nothing known: the draft keeps what it has.
        assert!(bills.addresses(&person(None, None)).is_none());
    }
}
