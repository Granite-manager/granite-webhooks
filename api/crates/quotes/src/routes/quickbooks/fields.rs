//! The custom fields a company's `QuickBooks` invoices carry, filled in for a
//! new invoice from its quote.
//!
//! Countertop shops keep the job itself in custom fields of the invoice
//! ("Room & Color": "Kitchen, Dallas White"; "Sink & Stove": "CP, F/S"; "Edge
//! & BS": "Eased, 4\""), each office in its own shorthand. An invoice made
//! from a quote used to leave them empty, so the office typed them again in
//! `QuickBooks` for every invoice.
//!
//! Which fields a company uses, and how it writes them, is read from its
//! newest invoices with the scan ([`CustomFields::read`]); for invoices made
//! here, the quote's lines are kept next to the values the office gave
//! ([`Filled`]). When Create invoice opens, the AI writes this quote's values
//! the company's way from those examples (`POST
//! /v1/quotes/{id}/invoice-fields`). They are text the rep sees and can
//! change before creating; no amount depends on them, and without the AI the
//! fields simply open empty.

use super::invoice::{rows, text};
use super::refuse_installer;
use super::rules;
use crate::ai::{Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// `QuickBooks`' longest custom field value.
pub(super) const VALUE_CHARS: usize = 31;
/// Fewest invoices a field must be filled on to be one the company uses.
const MIN_SEEN: usize = 3;
/// Distinct recent values kept of each field, as examples.
const EXAMPLES: usize = 15;
/// Quotes kept with the values the office gave their invoices.
const FILLED: usize = 8;
/// Lines of a quote shown to the AI, and characters of each.
const LINES: usize = 40;
const LINE_CHARS: usize = 160;
/// The editor waits this long for the values, then opens the fields empty.
const AI_TIMEOUT: Duration = Duration::from_secs(12);

const SYSTEM_PROMPT: &str = r#"You fill in the custom fields of a QuickBooks invoice for a countertop company, from the quote the invoice is made from.

Every company writes these fields in its own shorthand: its own order of parts, separators, abbreviations and capitalization (for example "CP" for a sink the customer provides, "F/S" for a free-standing stove, 4" for a 4 inch backsplash). Your values must read as if this company's office had typed them, so nobody has to retype them in QuickBooks.

You get one JSON object:
- "fields": the fields to fill. Each has a "name" and "examples": values this company gave the field on recent invoices, newest first. They show the format. They belong to other jobs.
- "filled_before": earlier quotes of this company ("lines") with the values its office gave their invoices ("values"). When present, this is the best guide to how quote lines turn into values. It may be empty.
- "quote": the lines of the quote to invoice now.

For each field, write the value this company would write for this quote:
- Match the field's examples in form: the same parts in the same order, separated and abbreviated the same way.
- Take the facts (room, color, sink, edge, backsplash and so on) only from "quote". Never carry a fact over from an example.
- When the quote does not say a part the field needs, write it the way the examples write a missing part (such as "N/A" or "no"). If the examples nearly always give that part the same value (a stove type, for instance), use that value.
- A value is one line of at most 31 characters. When it would be longer, shorten it the way the examples do.
- Leave a field out when you cannot tell what belongs in it. An empty field is better than a wrong one.

Return only JSON: {"values": {"<field name>": "<value>"}}"#;

/// One custom field as the company's invoices use it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FieldDef {
    /// `QuickBooks`' id of the field.
    pub id: String,
    pub name: String,
    /// How many of the scanned invoices fill it.
    pub filled: usize,
    /// Its distinct values, newest first.
    pub examples: Vec<String>,
}

/// A quote's lines with the values the office gave its invoice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Filled {
    pub lines: Vec<String>,
    /// By field name.
    pub values: BTreeMap<String, String>,
}

/// The custom fields of a company's scanned invoices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CustomFields {
    /// Invoices read.
    #[serde(default)]
    pub invoices: usize,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
    /// Quotes invoiced here whose invoice the office filled, newest first.
    #[serde(default)]
    pub filled_before: Vec<Filled>,
}

/// The text fields an invoice fills: (id, name, value).
fn string_fields(invoice: &Value) -> Vec<(String, String, String)> {
    rows(&invoice["CustomField"])
        .iter()
        .filter_map(|field| {
            let id = text(&field["DefinitionId"])?;
            let name = text(&field["Name"])?;
            let value = field["StringValue"].as_str()?.trim();
            (!value.is_empty()).then(|| (id, name.trim().to_string(), value.to_string()))
        })
        .collect()
}

impl CustomFields {
    /// Read from `QuickBooks`' own invoice objects, newest first. `drafts`
    /// are the draft lines of the invoices made here, by invoice id.
    pub fn read(invoices: &[Value], drafts: &HashMap<String, Vec<String>>) -> Self {
        let mut fields: Vec<FieldDef> = Vec::new();
        let mut filled_before = Vec::new();
        for invoice in invoices {
            let values = string_fields(invoice);
            for (id, name, value) in &values {
                let at = fields
                    .iter()
                    .position(|field| field.id == *id)
                    .unwrap_or_else(|| {
                        fields.push(FieldDef {
                            id: id.clone(),
                            name: name.clone(),
                            ..FieldDef::default()
                        });
                        fields.len() - 1
                    });
                let field = &mut fields[at];
                field.filled += 1;
                if field.examples.len() < EXAMPLES && !field.examples.contains(value) {
                    field.examples.push(value.clone());
                }
            }
            if filled_before.len() < FILLED
                && !values.is_empty()
                && let Some(lines) = text(&invoice["Id"]).and_then(|id| drafts.get(&id))
            {
                filled_before.push(Filled {
                    lines: lines
                        .iter()
                        .take(LINES)
                        .map(|line| line.chars().take(LINE_CHARS).collect())
                        .collect(),
                    values: values
                        .into_iter()
                        .map(|(_, name, value)| (name, value))
                        .collect(),
                });
            }
        }
        Self {
            invoices: invoices.len(),
            fields,
            filled_before,
        }
    }

    /// The fields the company's invoices usually fill: on at least
    /// [`MIN_SEEN`] invoices and half of them.
    pub fn usual(&self) -> Vec<&FieldDef> {
        self.fields
            .iter()
            .filter(|field| field.filled >= MIN_SEEN && field.filled * 2 >= self.invoices)
            .collect()
    }
}

/// A value as `QuickBooks` takes it: one line of at most [`VALUE_CHARS`].
pub(super) fn clean_value(value: &str) -> Option<String> {
    let value: String = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(VALUE_CHARS)
        .collect();
    let value = value.trim_end().to_string();
    (!value.is_empty()).then_some(value)
}

/// The values of the AI's reply for the fields asked, by field id.
fn values(reply: &Value, fields: &[&FieldDef]) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|field| {
            let value = reply["values"][&field.name]
                .as_str()
                .and_then(clean_value)?;
            Some((field.id.clone(), value))
        })
        .collect()
}

/// Body of `POST /v1/quotes/{id}/invoice-fields`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FieldsInput {
    /// The texts of the draft's lines, as the quote makes them.
    #[serde(default)]
    pub lines: Vec<String>,
}

/// One custom field of a new invoice.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftField {
    /// `QuickBooks`' id of the field: sent back with the value when the
    /// invoice is created.
    pub definition_id: String,
    /// Its name, e.g. "Room & Color".
    pub name: String,
    /// The value for this quote, written the company's way (at most 31
    /// characters); null when the AI could not tell, or did not answer.
    pub value: Option<String>,
    /// A few values the company gave it on recent invoices.
    pub examples: Vec<String>,
}

/// `POST /v1/quotes/{id}/invoice-fields`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftFields {
    /// The custom fields the company's invoices usually fill, in
    /// `QuickBooks`' order; empty when it uses none.
    pub fields: Vec<DraftField>,
}

/// Fill in a new invoice's custom fields from its quote
///
/// The custom fields the company's `QuickBooks` invoices usually fill (e.g.
/// "Room & Color", "Sink & Stove", "Edge & BS"), each with the value for
/// this quote written the way the company's own invoices write it. The AI
/// writes the values from the quote's lines, the field's recent values and
/// the values the office gave earlier invoices made from quotes. They are a
/// suggestion for the editor: send the ones to keep as `custom_fields` of
/// the invoice when creating it. Without the AI, or when it does not answer
/// in 12 seconds, the fields come with null values. Nothing is saved.
#[crate::openapi::path(
    post,
    path = "/v1/quotes/{id}/invoice-fields",
    operation_id = "fill_invoice_custom_fields",
    tag = "quickbooks",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = FieldsInput,
    responses(
        (status = 200, description = "The fields with this quote's values", body = DraftFields),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin, or is an installer", body = ErrorBody),
        (status = 404, description = "No such quote in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invoice_fields(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(quote_id): Path<i32>,
    JsonBody(input): JsonBody<FieldsInput>,
) -> ApiResult<Json<DraftFields>> {
    refuse_installer(&state, &user).await?;
    let company_id = user.company_id;
    let quote: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM quotes WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(quote_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    if quote.is_none() {
        return Err(ApiError::NotFound);
    }
    let custom = rules::custom_fields(&state, company_id).await?;
    let usual = custom.usual();
    let quote_lines: Vec<String> = input
        .lines
        .iter()
        .map(|line| line.trim().chars().take(LINE_CHARS).collect::<String>())
        .filter(|line| !line.is_empty())
        .take(LINES)
        .collect();
    let mut found = HashMap::new();
    if !usual.is_empty() && !quote_lines.is_empty() && state.ai.is_enabled() {
        let prompt = json!({
            "fields": usual
                .iter()
                .map(|field| json!({ "name": field.name, "examples": field.examples }))
                .collect::<Vec<_>>(),
            "filled_before": custom.filled_before,
            "quote": quote_lines,
        })
        .to_string();
        let chat = |model| {
            let mut chat = Chat::new(model)
                .message("system", SYSTEM_PROMPT)
                .message("user", &prompt);
            chat.json = true;
            chat.reasoning_effort = Some("low");
            chat.timeout = AI_TIMEOUT;
            chat
        };
        let caller = Caller {
            feature: "qbo_invoice_fields",
            user_id: Some(user.id),
            company_id: Some(company_id),
        };
        let reply = match state.ai.chat(&state.pool, caller, &chat(MINI_MODEL)).await {
            Err(err) if err.is_model_permission() => {
                state
                    .ai
                    .chat(&state.pool, caller, &chat(MINI_FALLBACK_MODEL))
                    .await
            }
            reply => reply,
        };
        match reply.map(|reply| serde_json::from_str::<Value>(reply.trim())) {
            Ok(Ok(reply)) => found = values(&reply, &usual),
            Ok(Err(_)) => tracing::warn!(company_id, "invoice fields: the AI answered no JSON"),
            // The fields open empty: the rep types them as before.
            Err(err) => tracing::warn!(error = %err, company_id, "invoice fields not filled"),
        }
    }
    Ok(Json(DraftFields {
        fields: usual
            .into_iter()
            .map(|field| DraftField {
                definition_id: field.id.clone(),
                name: field.name.clone(),
                value: found.remove(&field.id),
                examples: field.examples.iter().take(5).cloned().collect(),
            })
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoice(id: &str, fields: &[(&str, &str, &str)]) -> Value {
        json!({
            "Id": id,
            "CustomField": fields
                .iter()
                .map(|(def, name, value)| json!({
                    "DefinitionId": def, "Name": name, "Type": "StringType", "StringValue": value,
                }))
                .collect::<Vec<_>>(),
        })
    }

    fn scanned() -> Vec<Value> {
        vec![
            invoice(
                "18256",
                &[
                    ("1", "Room & Color", "Kitchen, Taj Beach"),
                    ("2", "Sink & Stove", "CP, F/S"),
                    ("3", "Edge & BS", "Eased, no"),
                ],
            ),
            invoice(
                "18200",
                &[
                    ("1", "Room & Color", "Kitchen, Dallas White"),
                    ("2", "Sink & Stove", "CP, F/S"),
                    ("3", "Edge & BS", "Eased, 4\""),
                    ("9", "PO", "77"),
                ],
            ),
            invoice(
                "18199",
                &[
                    ("1", "Room & Color", "Island, Calacatta Mont Blanc"),
                    ("3", "Edge & BS", " "),
                ],
            ),
            invoice("18198", &[("1", "Room & Color", "Kitchen, Taj Beach")]),
            json!({ "Id": "18197" }),
            json!({ "Id": "18196", "CustomField": [{ "DefinitionId": "4", "Name": "Paid", "Type": "BooleanType", "BooleanValue": true }] }),
        ]
    }

    #[test]
    fn a_scan_reads_the_fields_a_company_uses_and_how_it_writes_them() {
        let drafts = HashMap::from([(
            "18256".to_string(),
            vec![
                "Kitchen based @ 71.3 sqft Additional sqft of Taj Beach @ $70/sqft".to_string(),
                "1 Sink - CP sink".to_string(),
            ],
        )]);
        let custom = CustomFields::read(&scanned(), &drafts);
        assert_eq!(custom.invoices, 6);
        let usual = custom.usual();
        // On 4 and 2... of 6 invoices: only fields on at least half count.
        assert_eq!(
            usual
                .iter()
                .map(|field| (field.id.as_str(), field.name.as_str(), field.filled))
                .collect::<Vec<_>>(),
            [("1", "Room & Color", 4)]
        );
        assert_eq!(
            usual[0].examples,
            [
                "Kitchen, Taj Beach",
                "Kitchen, Dallas White",
                "Island, Calacatta Mont Blanc"
            ]
        );
        // The invoice made here keeps its quote's lines with its values.
        assert_eq!(custom.filled_before.len(), 1);
        assert_eq!(custom.filled_before[0].lines[1], "1 Sink - CP sink");
        assert_eq!(custom.filled_before[0].values["Sink & Stove"], "CP, F/S");
        // Kept and read back as it is stored.
        let stored: CustomFields =
            serde_json::from_str(&serde_json::to_string(&custom).unwrap()).unwrap();
        assert_eq!(stored, custom);
    }

    #[test]
    fn the_ai_values_are_kept_for_the_fields_asked_and_fit_quickbooks() {
        let custom = CustomFields::read(&scanned(), &HashMap::new());
        let fields: Vec<&FieldDef> = custom.fields.iter().collect();
        let reply = json!({ "values": {
            "Room & Color": "  Kitchen,   Calacatta Miraggio Cove Polished ",
            "Sink & Stove": "CP, F/S",
            "Edge & BS": " ",
            "Deposit": "500",
            "PO": 12,
        } });
        let found = values(&reply, &fields);
        // Squashed and cut at QuickBooks' 31 characters; an empty value, a
        // field not asked and a number are left out.
        assert_eq!(found.len(), 2);
        assert_eq!(found["1"], "Kitchen, Calacatta Miraggio Cov");
        assert_eq!(found["1"].chars().count(), VALUE_CHARS);
        assert_eq!(found["2"], "CP, F/S");
        assert!(values(&json!({ "values": "none" }), &fields).is_empty());
        assert_eq!(clean_value(" Eased,\n4\" "), Some("Eased, 4\"".into()));
        assert_eq!(clean_value("  "), None);
    }
}
