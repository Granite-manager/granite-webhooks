//! What an invoice made from a quote became in `QuickBooks`.
//!
//! [`super::rules::record`] keeps what the rep did to a draft before creating
//! the invoice. Offices also tidy an invoice afterwards, in `QuickBooks`
//! itself: they delete lines, reword them, set the class. Those changes are
//! as much the company's way as the ones made in the editor, so each time the
//! newest invoices are scanned the kept outcomes are checked against them: a
//! line since deleted is `removed`, a reworded one `changed`, and the class,
//! CC and product are the ones the invoice has now. The next drafts then
//! follow what the invoices ended up as.
//!
//! A line is found by the `QuickBooks` line id it got when the invoice was
//! created; a line whose id is gone while its very text is still on the
//! invoice (saved again as a new line) counts as kept. An invoice kept in
//! sync with its quote is rewritten from the quote (its text lines get new
//! ids each time), so only its class, CC and product are read back.

use super::invoice::{rows, text};
use super::rules::{Outcome, first_item, header_outcome, invoice_class, template};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::{HashMap, HashSet};

/// Newest outcomes checked against the scanned invoices.
const CHECKED: u32 = 5000;

/// One kept outcome.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct Kept {
    pub id: i32,
    pub qbo_invoice_id: String,
    /// `line`, `class`, `cc` or `item`.
    pub field: String,
    pub draft_text: Option<String>,
    pub outcome: String,
    pub saved_value: Option<String>,
    pub qbo_line_id: Option<String>,
    /// The quote's sales rep.
    pub rep_name: Option<String>,
    pub rep_email: Option<String>,
}

/// What a kept outcome is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Now {
    pub id: i32,
    pub outcome: Outcome,
    /// A product's name.
    pub draft_text: Option<String>,
}

fn trimmed(value: &Value) -> &str {
    value.as_str().map_or("", str::trim)
}

/// What a draft line is in the invoice now; none when it cannot be told
/// (the line was left out before the invoice was created, or its id was
/// never kept).
fn line_now(kept: &Kept, invoice: &Value) -> Option<Outcome> {
    let id = kept.qbo_line_id.as_deref()?;
    let drafted = kept.draft_text.as_deref()?.trim();
    let lines = rows(&invoice["Line"]);
    let now = lines
        .iter()
        .find(|line| text(&line["Id"]).as_deref() == Some(id))
        .map_or("", |line| trimmed(&line["Description"]));
    Some(if now == drafted {
        Outcome::new("kept", None)
    } else if !now.is_empty() {
        let to: String = template(drafted, now).chars().take(1000).collect();
        Outcome::new("changed", Some(&to))
    } else if lines
        .iter()
        .any(|line| trimmed(&line["Description"]) == drafted)
    {
        // Saved again under a new id (the editor's AI preview does that):
        // the same text is the same line.
        Outcome::new("kept", None)
    } else {
        Outcome::new("removed", None)
    })
}

/// The outcomes of `kept` that `invoice` (as `QuickBooks` has it now) no
/// longer bears out, as they are now. `lines` is off for an invoice whose
/// lines were rewritten since (kept in sync with its quote).
pub(super) fn differing(kept: &[Kept], invoice: &Value, staff: &[&str], lines: bool) -> Vec<Now> {
    kept.iter()
        .filter_map(|row| {
            let (outcome, draft_text) = match row.field.as_str() {
                "line" if lines => (line_now(row, invoice)?, row.draft_text.clone()),
                "class" => {
                    // The quote's rep, else (the quote is gone) anyone on staff.
                    let reps = row
                        .rep_name
                        .as_deref()
                        .map_or_else(|| staff.to_vec(), |own| vec![own]);
                    (
                        header_outcome(invoice_class(invoice).as_deref(), &reps),
                        None,
                    )
                }
                "cc" => (
                    header_outcome(
                        text(&invoice["BillEmailCc"]["Address"]).as_deref(),
                        &Vec::from_iter(row.rep_email.as_deref()),
                    ),
                    None,
                ),
                "item" => {
                    let item = first_item(invoice)?;
                    (Outcome::new("value", item.value.as_deref()), item.name)
                }
                _ => return None,
            };
            // A product is also told apart by its name.
            let named = row.field != "item" || draft_text == row.draft_text;
            let same = named
                && (outcome.outcome.as_str(), outcome.value.as_deref())
                    == (row.outcome.as_str(), row.saved_value.as_deref());
            (!same).then_some(Now {
                id: row.id,
                outcome,
                draft_text,
            })
        })
        .collect()
}

/// Brings the kept outcomes of the company's invoices in line with what
/// `invoices` (its newest, as `QuickBooks` has them) are now. Returns how
/// many changed.
pub(super) async fn reconcile(
    pool: &MySqlPool,
    company_id: i32,
    invoices: &[Value],
    staff: &[&str],
) -> sqlx::Result<usize> {
    let kept: Vec<Kept> = sqlx::query_as(
        "SELECT o.id, o.qbo_invoice_id, o.field, o.draft_text, o.outcome, o.saved_value,
                o.qbo_line_id, u.name AS rep_name, u.email AS rep_email
           FROM qbo_invoice_draft_outcomes o
           LEFT JOIN quotes q ON q.id = o.quote_id AND q.company_id = o.company_id
           LEFT JOIN users u ON u.id = q.salesperson_id AND u.is_deleted = 0
          WHERE o.company_id = ? AND o.qbo_invoice_id IS NOT NULL
          ORDER BY o.id DESC LIMIT ?",
    )
    .bind(company_id)
    .bind(CHECKED)
    .fetch_all(pool)
    .await?;
    if kept.is_empty() {
        return Ok(0);
    }
    let synced: Vec<String> = sqlx::query_scalar(
        "SELECT invoice_ids FROM quote_invoice_sync WHERE company_id = ? AND kind = 'qbo'",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    let synced: HashSet<&str> = synced
        .iter()
        .flat_map(|ids| ids.split(','))
        .map(str::trim)
        .collect();
    let mut by_invoice: HashMap<&str, Vec<Kept>> = HashMap::new();
    for row in &kept {
        by_invoice
            .entry(row.qbo_invoice_id.as_str())
            .or_default()
            .push(row.clone());
    }
    let mut changed = 0;
    for invoice in invoices {
        let Some(id) = text(&invoice["Id"]) else {
            continue;
        };
        let Some(rows) = by_invoice.get(id.as_str()) else {
            continue;
        };
        for now in differing(rows, invoice, staff, !synced.contains(id.as_str())) {
            sqlx::query(
                "UPDATE qbo_invoice_draft_outcomes
                    SET outcome = ?, saved_value = ?, draft_text = COALESCE(?, draft_text)
                  WHERE id = ? AND company_id = ?",
            )
            .bind(&now.outcome.outcome)
            .bind(&now.outcome.value)
            .bind(&now.draft_text)
            .bind(now.id)
            .bind(company_id)
            .execute(pool)
            .await?;
            changed += 1;
        }
    }
    if changed > 0 {
        tracing::info!(
            company_id,
            changed,
            "invoice draft outcomes read back from QuickBooks"
        );
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kept(id: i32, field: &str, text: Option<&str>, outcome: &str, line: Option<&str>) -> Kept {
        Kept {
            id,
            qbo_invoice_id: "9".into(),
            field: field.into(),
            draft_text: text.map(Into::into),
            outcome: outcome.into(),
            saved_value: None,
            qbo_line_id: line.map(Into::into),
            rep_name: Some("Marina Valera".into()),
            rep_email: Some("marina@acme.test".into()),
        }
    }

    /// The office deleted the fabrication line, reworded the edge and set the
    /// rep's class and another product, all in QuickBooks.
    fn tidied() -> Value {
        json!({
            "Id": "9",
            "Line": [
                { "Id": "1", "DetailType": "SalesItemLineDetail", "Description": "Kitchen based @ 59.2 sqft",
                  "SalesItemLineDetail": { "ItemRef": { "value": "30", "name": "Countertops" },
                                           "ClassRef": { "value": "5", "name": "Marina" } } },
                { "Id": "3", "DetailType": "DescriptionOnly", "Description": "Eased Edge 33.5 lf " },
                { "Id": "4", "DetailType": "DescriptionOnly", "Description": "1 Sink - 60/40" },
                { "DetailType": "SubTotalLineDetail" },
            ],
        })
    }

    fn drafted() -> Vec<Kept> {
        vec![
            kept(
                1,
                "line",
                Some("Kitchen based @ 59.2 sqft"),
                "kept",
                Some("1"),
            ),
            kept(2, "line", Some("59.2 sq ft Fabrication"), "kept", Some("2")),
            kept(
                3,
                "line",
                Some("33.5 lin ft Finished Edge - Eased"),
                "kept",
                Some("3"),
            ),
            kept(4, "line", Some("1 Sink - 60/40"), "kept", Some("4")),
            // Left out in the editor: nothing in QuickBooks to read back.
            kept(5, "line", Some("Laser Template"), "removed", None),
            kept(6, "class", None, "none", None),
            kept(7, "cc", None, "none", None),
            Kept {
                saved_value: Some("5".into()),
                draft_text: Some("Sales".into()),
                ..kept(8, "item", None, "value", None)
            },
        ]
    }

    #[test]
    fn what_the_office_changes_in_quickbooks_is_read_back() {
        let now = differing(&drafted(), &tidied(), &[], true);
        let got: Vec<(i32, &str, Option<&str>)> = now
            .iter()
            .map(|now| {
                (
                    now.id,
                    now.outcome.outcome.as_str(),
                    now.outcome.value.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (2, "removed", None),
                (3, "changed", Some("Eased Edge {0} lf")),
                // "Marina" is the class of the quote's rep, Marina Valera.
                (6, "sales_rep", None),
                (8, "value", Some("30")),
            ]
        );
        assert_eq!(now[3].draft_text.as_deref(), Some("Countertops"));
        // Read again, nothing differs.
        let mut settled = drafted();
        for now in &now {
            let row = settled.iter_mut().find(|row| row.id == now.id).unwrap();
            row.outcome.clone_from(&now.outcome.outcome);
            row.saved_value.clone_from(&now.outcome.value);
            if row.field == "item" {
                row.draft_text.clone_from(&now.draft_text);
            }
        }
        assert!(differing(&settled, &tidied(), &[], true).is_empty());
    }

    #[test]
    fn a_line_saved_again_under_a_new_id_is_still_kept() {
        let mut invoice = tidied();
        invoice["Line"][2]["Id"] = json!("40");
        let now = differing(&drafted(), &invoice, &[], true);
        assert!(now.iter().all(|now| now.id != 4));
        assert_eq!(now.len(), 4);
    }

    #[test]
    fn an_invoice_rewritten_from_its_quote_keeps_its_line_outcomes() {
        // Kept in sync: its text lines have new ids, which says nothing
        // about what the office did.
        let now = differing(&drafted(), &tidied(), &[], false);
        let ids: Vec<i32> = now.iter().map(|now| now.id).collect();
        assert_eq!(ids, [6, 8]);
    }
}
