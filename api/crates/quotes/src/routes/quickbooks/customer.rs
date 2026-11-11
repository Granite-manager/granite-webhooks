//! The `QuickBooks` customer a CRM customer's invoices are billed to.
//!
//! A customer the CRM creates in `QuickBooks` gets what the CRM knows: first
//! and last name apart (not only the display name), the company name, the
//! phones, the email and the billing address. A customer `QuickBooks`
//! already had keeps everything it has; only what it lacks is filled in.

use super::invoice::{Address, text};
use serde_json::{Map, Value, json};

/// `QuickBooks`' longest first, middle, last and company names.
const NAME_CHARS: usize = 100;
/// Its longest title and suffix.
const AFFIX_CHARS: usize = 16;
/// Its longest phone number.
const PHONE_CHARS: usize = 30;

/// What the CRM knows about a customer.
#[derive(Debug, Default, Clone, sqlx::FromRow)]
pub(super) struct CustomerRow {
    pub name: Option<String>,
    pub qbo_id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub phone_2: Option<String>,
    pub company_name: Option<String>,
    pub address: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
}

fn filled(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

impl CustomerRow {
    /// A builder, designer or other business: its address is its office.
    pub fn is_business(&self) -> bool {
        filled(self.company_name.as_deref()).is_some()
            || filled(self.name.as_deref()).is_some_and(looks_like_business)
    }

    /// The customer's own address, as the CRM keeps it (street, then city,
    /// state and ZIP apart or in the street's text).
    pub fn own_address(&self) -> Option<Address> {
        let street = filled(self.address.as_deref())?;
        let city = filled(self.city.as_deref());
        let state = filled(self.state.as_deref());
        let zip = filled(self.postal_code.as_deref());
        // The street holds the whole address ("1 Main St, Carmel, IN 46032").
        if (city.is_none() && state.is_none() && zip.is_none())
            || street.contains('\n')
            || zip.is_some_and(|zip| street.contains(zip))
        {
            return parse_address(street);
        }
        Some(Address {
            line1: Some(street.to_string()),
            city: city.map(str::to_string),
            country_sub_division_code: state.map(|state| {
                if state.len() == 2 {
                    state.to_uppercase()
                } else {
                    state.to_string()
                }
            }),
            postal_code: zip.map(str::to_string),
            ..Address::default()
        })
    }
}

/// Words a person's name never ends or goes with.
const BUSINESS_WORDS: [&str; 30] = [
    "llc",
    "inc",
    "corp",
    "corporation",
    "co",
    "company",
    "ltd",
    "lp",
    "llp",
    "pllc",
    "group",
    "construction",
    "builders",
    "homes",
    "properties",
    "remodeling",
    "renovations",
    "contracting",
    "contractors",
    "kitchens",
    "cabinets",
    "cabinetry",
    "interiors",
    "designs",
    "services",
    "enterprises",
    "associates",
    "realty",
    "plumbing",
    "restoration",
];

fn looks_like_business(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric())
        .any(|word| BUSINESS_WORDS.contains(&word.to_lowercase().as_str()))
}

/// A person's name in the parts `QuickBooks` keeps.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct NameParts {
    pub title: Option<String>,
    pub given: Option<String>,
    pub family: Option<String>,
    pub suffix: Option<String>,
}

const TITLES: [&str; 6] = ["mr", "mrs", "ms", "miss", "dr", "mx"];
const SUFFIXES: [&str; 6] = ["jr", "sr", "ii", "iii", "iv", "esq"];

fn bare(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

fn cut(text: &str, max: usize) -> Option<String> {
    let text: String = text.trim().chars().take(max).collect();
    (!text.is_empty()).then_some(text)
}

/// "John Smith" → John / Smith; "John and Mary Smith" → John and Mary /
/// Smith; "Smith, John" → John / Smith; "Dr. Ann Lee Jr." → Dr. / Ann / Lee /
/// Jr. One word is a first name.
pub(super) fn name_parts(name: &str) -> NameParts {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    // "Smith, John": the family name first.
    if let Some((family, given)) = name.split_once(',')
        && !family.trim().is_empty()
        && !given.trim().is_empty()
        && !given.contains(',')
        && !SUFFIXES.contains(&bare(given).as_str())
    {
        let mut parts = name_parts(given);
        parts.family = cut(family, NAME_CHARS);
        return parts;
    }
    let mut words: Vec<&str> = name.split(' ').filter(|word| !word.is_empty()).collect();
    let mut parts = NameParts::default();
    if words.len() > 1 && TITLES.contains(&bare(words[0]).as_str()) {
        parts.title = cut(words.remove(0), AFFIX_CHARS);
    }
    if words.len() > 1
        && let Some(last) = words.last()
        && SUFFIXES.contains(&bare(last).as_str())
    {
        parts.suffix = cut(last.trim_matches(','), AFFIX_CHARS);
        words.pop();
    }
    if let Some(last) = words.last_mut() {
        *last = last.trim_end_matches(',');
    }
    match words.as_slice() {
        [] => {}
        [only] => parts.given = cut(only, NAME_CHARS),
        [given @ .., family] => {
            parts.given = cut(&given.join(" "), NAME_CHARS);
            parts.family = cut(family, NAME_CHARS);
        }
    }
    parts
}

/// A phone as `QuickBooks` shows them: "(317) 555-0100" for a US number,
/// anything else as it is written.
pub(super) fn phone(value: Option<&str>) -> Option<String> {
    let value = filled(value)?;
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    let national = match digits.len() {
        11 if digits.starts_with('1') => &digits[1..],
        10 => digits.as_str(),
        _ => return cut(value, PHONE_CHARS),
    };
    Some(format!(
        "({}) {}-{}",
        &national[..3],
        &national[3..6],
        &national[6..]
    ))
}

fn is_zip(part: &str) -> bool {
    let (five, plus) = part.split_once('-').unwrap_or((part, ""));
    five.len() == 5
        && five.bytes().all(|b| b.is_ascii_digit())
        && (plus.is_empty() || (plus.len() == 4 && plus.bytes().all(|b| b.is_ascii_digit())))
}

/// "IN" or "IN 46032".
fn state_zip(part: &str) -> Option<(String, Option<String>)> {
    let mut words = part.split_whitespace();
    let state = words.next()?;
    let zip = words.next();
    if words.next().is_some()
        || state.len() != 2
        || !state.bytes().all(|b| b.is_ascii_alphabetic())
        || zip.is_some_and(|zip| !is_zip(zip))
    {
        return None;
    }
    Some((state.to_uppercase(), zip.map(str::to_string)))
}

/// "1 Main St, Carmel, IN 46032" (or "IN, 46032", no ZIP, or the city on a
/// second line) as an address; anything else stays on line 1. The CRM's
/// `invoiceAddress`.
pub(super) fn parse_address(text: &str) -> Option<Address> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = text
        .split([',', '\n'])
        .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|part| !part.is_empty())
        .collect();
    if parts
        .last()
        .is_some_and(|last| matches!(last.to_lowercase().as_str(), "us" | "usa" | "united states"))
    {
        parts.pop();
    }
    // "IN, 46032": the ZIP after its own comma.
    if parts.len() >= 4 && parts.last().is_some_and(|last| is_zip(last)) {
        let zip = parts.pop().unwrap_or_default();
        if let Some(state) = parts.last_mut() {
            *state = format!("{state} {zip}");
        }
    }
    if parts.len() >= 3
        && let Some((state, zip)) = parts.last().and_then(|last| state_zip(last))
    {
        let city = parts[parts.len() - 2].clone();
        return Some(Address {
            line1: Some(parts[..parts.len() - 2].join(", ")),
            city: Some(city),
            country_sub_division_code: Some(state),
            postal_code: zip,
            ..Address::default()
        });
    }
    Some(Address {
        line1: Some(parts.join(", ")),
        ..Address::default()
    })
}

/// A `QuickBooks` display name: no `:` (it nests customers), tabs or new lines.
pub(super) fn display_name(name: &str) -> String {
    name.split([':', '\t', '\n', '\r', ' '])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect()
}

/// What the CRM knows of the customer, as `QuickBooks` customer fields
/// (without the display name). `job` is the quote's job address: a person's
/// billing address when the CRM has no address of their own (a business is
/// billed at its office, never at the job).
pub(super) fn known_fields(row: &CustomerRow, job: Option<&Address>) -> Map<String, Value> {
    let mut fields = Map::new();
    // No `:`, tabs or new lines, which QuickBooks refuses in names.
    let name = display_name(row.name.as_deref().unwrap_or_default());
    let name = name.as_str();
    let company = filled(row.company_name.as_deref()).map(display_name);
    let company = company.as_deref().filter(|company| !company.is_empty());
    // The customer's name is a business, not its contact: nobody to split.
    let named_as_business = looks_like_business(name)
        || company.is_some_and(|company| company.eq_ignore_ascii_case(name));
    if let Some(company) = company
        .or_else(|| named_as_business.then_some(name))
        .and_then(|company| cut(company, NAME_CHARS))
    {
        fields.insert("CompanyName".into(), json!(company));
    }
    if !named_as_business {
        let parts = name_parts(name);
        for (key, value) in [
            ("Title", parts.title),
            ("GivenName", parts.given),
            ("FamilyName", parts.family),
            ("Suffix", parts.suffix),
        ] {
            if let Some(value) = value {
                fields.insert(key.into(), json!(value));
            }
        }
    }
    if let Some(email) = filled(row.email.as_deref()) {
        fields.insert("PrimaryEmailAddr".into(), json!({ "Address": email }));
    }
    let first = phone(row.phone.as_deref());
    let second = phone(row.phone_2.as_deref()).filter(|second| Some(second) != first.as_ref());
    match (first, second) {
        (Some(first), second) => {
            fields.insert("PrimaryPhone".into(), json!({ "FreeFormNumber": first }));
            if let Some(second) = second {
                fields.insert("AlternatePhone".into(), json!({ "FreeFormNumber": second }));
            }
        }
        (None, Some(second)) => {
            fields.insert("PrimaryPhone".into(), json!({ "FreeFormNumber": second }));
        }
        (None, None) => {}
    }
    let own = row.own_address();
    let billing = own.as_ref().or_else(|| job.filter(|_| !row.is_business()));
    if let Some(address) = billing.and_then(Address::to_qbo) {
        fields.insert("BillAddr".into(), address);
    }
    fields
}

fn has_name(current: &Value) -> bool {
    ["GivenName", "FamilyName"]
        .iter()
        .any(|key| text(&current[*key]).is_some())
}

fn has_phone(current: &Value) -> bool {
    ["PrimaryPhone", "Mobile", "AlternatePhone"]
        .iter()
        .any(|key| text(&current[*key]["FreeFormNumber"]).is_some())
}

fn has_address(current: &Value) -> bool {
    ["Line1", "Line2", "City", "PostalCode"]
        .iter()
        .any(|key| text(&current["BillAddr"][*key]).is_some())
}

/// The sparse update that fills what the `QuickBooks` customer `current`
/// lacks from `known`; none when it lacks nothing the CRM knows. A name,
/// phone or address it already has is never replaced, in whole or in part.
pub(super) fn completion(current: &Value, known: &Map<String, Value>) -> Option<Value> {
    let id = text(&current["Id"])?;
    let sync_token = text(&current["SyncToken"])?;
    let mut update = Map::new();
    for (key, value) in known {
        let lacking = match key.as_str() {
            "Title" | "GivenName" | "FamilyName" | "Suffix" => !has_name(current),
            "PrimaryPhone" | "AlternatePhone" => !has_phone(current),
            "BillAddr" => !has_address(current),
            "PrimaryEmailAddr" => text(&current["PrimaryEmailAddr"]["Address"]).is_none(),
            // A company name would change how an existing customer is filed.
            _ => false,
        };
        if lacking {
            update.insert(key.clone(), value.clone());
        }
    }
    if update.is_empty() {
        return None;
    }
    update.insert("Id".into(), json!(id));
    update.insert("SyncToken".into(), json!(sync_token));
    update.insert("sparse".into(), json!(true));
    Some(Value::Object(update))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(title: &str, given: &str, family: &str, suffix: &str) -> NameParts {
        let some = |value: &str| (!value.is_empty()).then(|| value.to_string());
        NameParts {
            title: some(title),
            given: some(given),
            family: some(family),
            suffix: some(suffix),
        }
    }

    #[test]
    fn a_name_splits_into_first_and_last() {
        assert_eq!(name_parts("John Smith"), parts("", "John", "Smith", ""));
        assert_eq!(
            name_parts("  John  and Mary   Smith "),
            parts("", "John and Mary", "Smith", "")
        );
        assert_eq!(name_parts("Smith, John"), parts("", "John", "Smith", ""));
        assert_eq!(
            name_parts("Dr. Ann Lee Jr."),
            parts("Dr.", "Ann", "Lee", "Jr.")
        );
        assert_eq!(name_parts("Ann Lee, Jr"), parts("", "Ann", "Lee", "Jr"));
        assert_eq!(name_parts("Madonna"), parts("", "Madonna", "", ""));
        assert_eq!(name_parts(" "), NameParts::default());
    }

    #[test]
    fn phones_read_like_quickbooks_writes_them() {
        assert_eq!(
            phone(Some("317-555-0100")).as_deref(),
            Some("(317) 555-0100")
        );
        assert_eq!(
            phone(Some("+1 317 555 0100")).as_deref(),
            Some("(317) 555-0100")
        );
        assert_eq!(
            phone(Some("+44 20 7946 0958")).as_deref(),
            Some("+44 20 7946 0958")
        );
        assert_eq!(phone(Some("  ")), None);
        assert_eq!(phone(None), None);
    }

    fn address(text: &str) -> (String, String, String, String) {
        let found = parse_address(text).unwrap();
        (
            found.line1.unwrap_or_default(),
            found.city.unwrap_or_default(),
            found.country_sub_division_code.unwrap_or_default(),
            found.postal_code.unwrap_or_default(),
        )
    }

    fn quad(a: &str, b: &str, c: &str, d: &str) -> (String, String, String, String) {
        (a.into(), b.into(), c.into(), d.into())
    }

    #[test]
    fn an_address_on_one_line_splits_into_its_parts() {
        let carmel = quad("1 Main St", "Carmel", "IN", "46032");
        assert_eq!(address("1 Main St, Carmel, IN 46032"), carmel);
        assert_eq!(address("1 Main St, Carmel, in, 46032, USA"), carmel);
        assert_eq!(address("1 Main St\nCarmel, IN 46032"), carmel);
        assert_eq!(
            address("1 Main St, Apt 4, Carmel, IN"),
            quad("1 Main St, Apt 4", "Carmel", "IN", "")
        );
        assert_eq!(
            address("Lot 12 by the lake"),
            quad("Lot 12 by the lake", "", "", "")
        );
        assert!(parse_address("  ").is_none());
    }

    fn alice() -> CustomerRow {
        CustomerRow {
            name: Some("Alice Lead".into()),
            email: Some(" alice@example.com ".into()),
            phone: Some("317-555-0100".into()),
            phone_2: Some("3175550101".into()),
            address: Some("1 Main St".into()),
            city: Some("Carmel".into()),
            state: Some("IN".into()),
            postal_code: Some("46032".into()),
            ..CustomerRow::default()
        }
    }

    #[test]
    fn a_new_customer_gets_what_the_crm_knows() {
        let fields = Value::Object(known_fields(&alice(), None));
        assert_eq!(fields["GivenName"], "Alice");
        assert_eq!(fields["FamilyName"], "Lead");
        assert_eq!(fields["PrimaryEmailAddr"]["Address"], "alice@example.com");
        assert_eq!(fields["PrimaryPhone"]["FreeFormNumber"], "(317) 555-0100");
        assert_eq!(fields["AlternatePhone"]["FreeFormNumber"], "(317) 555-0101");
        assert_eq!(
            fields["BillAddr"],
            json!({ "Line1": "1 Main St", "City": "Carmel",
                    "CountrySubDivisionCode": "IN", "PostalCode": "46032" })
        );
        assert!(fields.get("CompanyName").is_none());

        // No address of their own: a person is billed at the job.
        let job = parse_address("9 Oak Ave, Fishers, IN 46037");
        let homeless = CustomerRow {
            address: None,
            ..alice()
        };
        let fields = Value::Object(known_fields(&homeless, job.as_ref()));
        assert_eq!(fields["BillAddr"]["Line1"], "9 Oak Ave");

        // A builder is a company with a contact, never billed at the job.
        let builder = CustomerRow {
            company_name: Some("Cedarbrook Properties".into()),
            address: None,
            ..alice()
        };
        let fields = Value::Object(known_fields(&builder, job.as_ref()));
        assert_eq!(fields["CompanyName"], "Cedarbrook Properties");
        assert_eq!(fields["GivenName"], "Alice");
        assert!(fields.get("BillAddr").is_none());

        // A business kept as the customer's name has no first and last name.
        let llc = CustomerRow {
            name: Some("Stone Homes LLC".into()),
            ..CustomerRow::default()
        };
        let fields = Value::Object(known_fields(&llc, None));
        assert_eq!(fields["CompanyName"], "Stone Homes LLC");
        assert!(fields.get("GivenName").is_none());
    }

    #[test]
    fn a_quickbooks_customer_only_gets_what_it_lacks() {
        let known = known_fields(&alice(), None);
        let bare = json!({ "Id": "42", "SyncToken": "3", "DisplayName": "Alice Lead" });
        let update = completion(&bare, &known).unwrap();
        assert_eq!(update["Id"], "42");
        assert_eq!(update["SyncToken"], "3");
        assert_eq!(update["sparse"], true);
        assert_eq!(update["GivenName"], "Alice");
        assert_eq!(update["BillAddr"]["City"], "Carmel");
        assert!(update.get("DisplayName").is_none());

        // What QuickBooks has stays, even in part: a first name alone, a
        // mobile number, a billing city.
        let partly = json!({
            "Id": "42", "SyncToken": 3, "GivenName": "Alicia",
            "Mobile": { "FreeFormNumber": "555-0199" },
            "BillAddr": { "City": "Fishers" },
            "PrimaryEmailAddr": { "Address": "a@x.test" },
        });
        assert_eq!(completion(&partly, &known), None);
        assert_eq!(completion(&json!({ "Id": "42" }), &known), None);
    }
}
