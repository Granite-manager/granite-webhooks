//! What a sold project looks like in Raptor.
//!
//! The job name ("Last First Room"), its Material and Cutout entries, the
//! customer, the address and the "Notes" note in the office's fixed template.
//! Pure; `sync` reads the CRM and `client` writes this into Raptor. The same
//! rules as granite-front's `scripts/raptor/raptorJob.ts`.

use crate::quote_pricing::doc::{Area, CutoutKind, ItemKind, QuoteDoc, QuoteItem, SinkType};
use crate::quote_pricing::takeoff::{RoundedTakeoff, area_takeoff, round_takeoff, round1};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

/// What the CRM knows about a sold project.
pub struct Source<'a> {
    /// The CRM customer, "First Last".
    pub customer_name: &'a str,
    /// The customer's company (a builder's "TBG"), if any.
    pub company_name: Option<&'a str>,
    pub phone: Option<&'a str>,
    /// The job address, as the sold quote has it.
    pub address: Option<&'a str>,
    /// The quote's salesperson ("Liza", "Kyle Smith").
    pub salesperson: Option<&'a str>,
    pub doc: &'a QuoteDoc,
    /// The color option the customer bought (`customer_accepted_option_id`).
    pub option_index: usize,
    /// The project's notes, oldest first. "Layout yes" / "Layout no" sets Layout.
    pub project_notes: &'a [String],
    /// The price list's Material Rounding is "up".
    pub sq_ft_up: bool,
    /// The project's title ("Northpoint Management - unit 3046"); the office
    /// names the job by it when it is more than the customer's name.
    pub project_title: Option<&'a str>,
    /// The titles of the project's events: "Deborah Lashbrook - Pick up" is a pick-up.
    pub event_titles: &'a [String],
    /// The price list's edge names, its default ("Flat") first. An edge the
    /// list does not have ("55") is that default, as the office writes it.
    pub edge_names: &'a [String],
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Material {
    pub brand: String,
    pub color: String,
    pub edge: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cutout {
    pub model: String,
    pub quantity: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub street: String,
    pub city: String,
    pub state: String,
    pub zip: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// "Last First Room", as the office names jobs.
    pub name: String,
    pub sq_ft: f64,
    pub materials: Vec<Material>,
    pub cutouts: Vec<Cutout>,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub customer_name: String,
    pub phone: String,
    pub address: Address,
    pub jobs: Vec<Job>,
}

static FULL_HEIGHT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)full\s*-?\s*height|\bfhbs\b|\bfh\b").expect("regex"));
static PICK_UP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bpick\s*-?\s*up\b|\bno\s+install").expect("regex"));
// "Layout yes", "Layout: no", "Layout - yes", "L/O YES".
static LAYOUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\blayout|\bl\s*/\s*o)\s*[:\-–]?\s*(yes|no)\b").expect("regex")
});
/// A name that starts with a builder's short name ("IHDC Kim Keyser") stays as it is.
static ACRONYM_FIRST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z]{3,}\s+\S*[a-z]").expect("regex"));
static ZIP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{5})(?:-\d{4})?\s*$").expect("regex"));
static TRAILING_STATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*\S)\s+([A-Za-z]{2})$").expect("regex"));
static TWO_LETTERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]{2}$").expect("regex"));
static SUPPORTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)corbel|support|bracket").expect("regex"));
static FAUCET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)faucet").expect("regex"));
static HOLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)hole").expect("regex"));
static SOAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)soap").expect("regex"));
static BOARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)cutting\s*board").expect("regex"));
static TEAR_OUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bt/o\b|tear\s*-?\s*out").expect("regex"));
static BY_CUSTOMER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bby\s+(the\s+)?customer|\bcustomer\s+(does|will|provides?|supplie[sd])")
        .expect("regex")
});
// A business kept as its name ("GTP Holdings"), not turned "Last First";
// never a word that is also a surname ("Erin Builder").
static BUSINESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(llc|inc|corp|company|holdings|properties|construction|builders|handyman|remodeling|investments|realty|management|enterprises|services)\b")
        .expect("regex")
});
/// "No existing countertops to tear out", "Wall Tile Tear Out": not a countertop tear out.
static NOT_TEAR_OUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*no\b|\bno\s+existing|\bwall\s*tile|\bbacksplash").expect("regex")
});
static CABINET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)cabinet").expect("regex"));
/// "Customer Provides a sink": the office writes "CP".
static CUSTOMER_SINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*customer\s+provides?\b").expect("regex"));
/// Area names that say nothing about the room ("Area #1", "Countertops").
static GENERIC_AREA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(area\s*#?\s*\d*|countertops?|kitchen\s+countertops?|main)\s*$")
        .expect("regex")
});
/// Dishwasher brackets hold no countertop.
static NOT_SUPPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bdw\b|dish\s*washer").expect("regex"));
static NO_BACKSPLASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bno\s+(back\s*)?splash").expect("regex"));
static DISCOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)discount|%|\boff\b").expect("regex"));
static SEALER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)sealer").expect("regex"));
static SEAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)seam").expect("regex"));
static PHANTOM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)phantom").expect("regex"));

const RULE: &str = "___________________";

/// "Mike Pendley" → "Pendley Mike"; "Marcus and Mary ARMSTRONG" → "ARMSTRONG Marcus and Mary".
pub fn last_name_first(name: &str) -> String {
    let mut words: Vec<&str> = name.split_whitespace().collect();
    let Some(last) = words.pop() else {
        return String::new();
    };
    std::iter::once(last)
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ")
}

/// "8435 Smithfield Lane, Indianapolis, IN, 46237" → street, city, state, ZIP.
pub fn split_address(raw: Option<&str>) -> Address {
    let text = raw.map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return Address::default();
    }
    let zip = ZIP
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let rest = if zip.is_empty() {
        text
    } else {
        text[..text.rfind(zip.as_str()).unwrap_or(text.len())]
            .trim_end_matches(|c: char| c == ',' || c.is_whitespace())
    };
    let mut parts: Vec<String> = rest
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let mut state = String::new();
    if parts.len() > 1 {
        let tail = parts[parts.len() - 1].clone();
        if TWO_LETTERS.is_match(&tail) {
            state = tail.to_uppercase();
            parts.pop();
        } else if let Some(c) = TRAILING_STATE.captures(&tail) {
            state = c[2].to_uppercase();
            let len = parts.len();
            parts[len - 1] = c[1].to_string();
        }
    }
    if parts.len() == 1 && !parts[0].chars().any(|c| c.is_ascii_digit()) {
        return Address {
            city: parts.remove(0),
            state,
            zip,
            ..Address::default()
        };
    }
    let city = if parts.len() > 1 {
        parts.pop().unwrap_or_default()
    } else {
        String::new()
    };
    Address {
        street: parts.join(", "),
        city,
        state,
        zip,
    }
}

/// The last "Layout yes/no" in the project notes, as the office writes it.
pub fn layout_from_notes(notes: &[String]) -> Option<&'static str> {
    notes
        .iter()
        .flat_map(|note| LAYOUT.captures_iter(note))
        .last()
        .map(|c| {
            if c[1].eq_ignore_ascii_case("yes") {
                "Yes"
            } else {
                "no"
            }
        })
}

fn words(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The project's title when it says more than the customer's name ("Brad
/// Wilson", "Brad Wilson project" say nothing).
fn own_title<'a>(title: Option<&'a str>, customer: &str) -> Option<&'a str> {
    let title = title.map(str::trim).filter(|t| !t.is_empty())?;
    let (t, c) = (words(title), words(customer));
    (t != c && t != format!("{c} project")).then_some(title)
}

/// `title` without the customer's name in front ("Drew Fairchild Shower" →
/// "Shower"); `None` when it does not start with it.
fn after_name<'a>(title: &'a str, customer: &str) -> Option<&'a str> {
    let name: Vec<&str> = customer.split_whitespace().collect();
    let mut rest = title.trim_start();
    for word in &name {
        let next = rest.get(..word.len())?;
        let after = &rest[word.len()..];
        if !next.eq_ignore_ascii_case(word) || after.starts_with(char::is_alphanumeric) {
            return None;
        }
        rest = after.trim_start();
    }
    (!name.is_empty()).then_some(rest)
}

/// The price list's spelling of `edge`; its default edge when `edge` has no
/// letters ("55", nothing); else `edge` as the seller typed it.
fn known_edge(edge: &str, names: &[String]) -> String {
    let edge = edge.trim();
    if let Some(name) = names.iter().find(|n| n.trim().eq_ignore_ascii_case(edge)) {
        return name.trim().to_string();
    }
    match names.first() {
        Some(default) if !edge.chars().any(char::is_alphabetic) => default.trim().to_string(),
        _ => edge.to_string(),
    }
}

/// A number as JavaScript prints it: 4 → "4", 19.2 → "19.2".
fn num(v: f64) -> String {
    let r = round1(v);
    #[allow(clippy::float_cmp, clippy::cast_possible_truncation)]
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

fn inches(v: f64) -> String {
    format!("{}\"", num(v))
}

fn sink_model(model: Option<&str>, kind: SinkType, width: f64) -> String {
    if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
        if CUSTOMER_SINK.is_match(model) {
            return "CP".into();
        }
        return model.to_string();
    }
    let kind = match kind {
        SinkType::Farmer => "Farmer",
        SinkType::DropIn => "Drop-in",
        SinkType::Undermount => "Undermount",
    };
    format!("{} {kind} Sink Cutout", inches(width))
}

const fn yes_no(v: bool) -> &'static str {
    if v { "Yes" } else { "no" }
}

fn list(values: &[String]) -> String {
    if values.is_empty() {
        "no".into()
    } else {
        values.join(", ")
    }
}

/// The item's name or option says what `re` matches.
fn mentions(item: &QuoteItem, re: &Regex) -> bool {
    re.is_match(&item.name) || item.option.as_deref().is_some_and(|o| re.is_match(o))
}

#[allow(clippy::too_many_lines)]
pub fn plan(src: &Source) -> Plan {
    let doc = src.doc;
    let option = doc
        .color_options
        .get(src.option_index)
        .or_else(|| doc.color_options.first());
    let items: Vec<&QuoteItem> = doc
        .items
        .iter()
        .filter(|i| i.kind != ItemKind::Text)
        .collect();
    let named = |re: &Regex| -> Vec<&QuoteItem> {
        items
            .iter()
            .copied()
            .filter(|i| re.is_match(&i.name) || i.option.as_deref().is_some_and(|o| re.is_match(o)))
            .collect()
    };
    let said = |re: &Regex| -> Vec<String> {
        named(re)
            .into_iter()
            .filter(|i| !mentions(i, &BY_CUSTOMER))
            .map(|i| {
                i.option
                    .as_deref()
                    .map(str::trim)
                    .filter(|o| !o.is_empty())
                    .unwrap_or_else(|| i.name.trim())
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect()
    };

    let cutouts = doc
        .counters
        .iter()
        .flat_map(|c| &c.runs)
        .flat_map(|r| &r.cutouts);
    let pick_up = doc.texts().any(|t| PICK_UP.is_match(t))
        // "Black Friday Discount (10% pick up)" is a price, not a pick-up.
        || doc
            .items
            .iter()
            .any(|i| mentions(i, &PICK_UP) && !mentions(i, &DISCOUNT))
        || src.project_notes.iter().any(|n| PICK_UP.is_match(n))
        // "Deborah Lashbrook - Pick up"; a "Cabinet Pick up" is no countertop's.
        || src
            .event_titles
            .iter()
            .any(|t| PICK_UP.is_match(t) && !CABINET.is_match(t));

    let (full_height, main): (Vec<&Area>, Vec<&Area>) = doc
        .areas
        .iter()
        .partition(|a| FULL_HEIGHT.is_match(&a.name));
    // One unnamed area ("Area #1") is the kitchen, as the office writes it
    // ("Kitchen(59) Vanity(31.6)"); several keep their own names.
    let one_generic = main
        .iter()
        .filter(|a| GENERIC_AREA.is_match(&a.name))
        .count()
        == 1;
    let room_of = |a: &Area| -> String {
        let name = a.name.trim();
        if one_generic && GENERIC_AREA.is_match(name) {
            "Kitchen".into()
        } else {
            name.to_string()
        }
    };
    let takeoffs: HashMap<&str, RoundedTakeoff> = doc
        .areas
        .iter()
        .map(|a| {
            (
                a.id.as_str(),
                round_takeoff(&area_takeoff(doc, &a.id), src.sq_ft_up),
            )
        })
        .collect();
    // Countertop plus splash, waterfall and miter pieces, as the sale counts it.
    #[allow(clippy::cast_precision_loss)]
    let sq_ft = |areas: &[&Area]| {
        round1(areas.iter().fold(0.0, |sum, a| {
            takeoffs
                .get(a.id.as_str())
                .map_or(0.0, |t| t.fabrication_sq_ft)
                .mul_add(a.quantity.max(1) as f64, sum)
        }))
    };

    let mut heights: Vec<f64> = main
        .iter()
        .filter_map(|a| takeoffs.get(a.id.as_str()))
        .flat_map(|t| t.splash.iter().map(|s| s.height))
        .collect();
    heights.sort_by(f64::total_cmp);
    heights.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let texts: Vec<&str> = doc.texts().collect();
    let backsplash = if !full_height.is_empty() || texts.iter().any(|t| FULL_HEIGHT.is_match(t)) {
        "FH".to_string()
    } else if heights.is_empty() || texts.iter().any(|t| NO_BACKSPLASH.is_match(t)) {
        "no".to_string()
    } else {
        heights
            .iter()
            .map(|h| inches(*h))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let waterfall = takeoffs.values().any(|t| !t.waterfall.is_empty());
    let stove = if cutouts.clone().any(|c| c.kind == CutoutKind::Cooktop) {
        "C/T"
    } else {
        "F/S"
    };
    #[allow(clippy::cast_possible_truncation)]
    let supports = named(&SUPPORTS)
        .iter()
        .filter(|i| !mentions(i, &NOT_SUPPORT))
        .fold(0.0, |sum, i| {
            sum + if i.quantity > 0.0 { i.quantity } else { 1.0 }
        }) as i64;
    let faucet: Vec<String> = said(&FAUCET)
        .into_iter()
        .filter(|f| !HOLE.is_match(f))
        .collect();
    let soap = said(&SOAP);
    let board = said(&BOARD);
    // "Tear Out to be completed by customer" is not ours to do.
    let tear_out = named(&TEAR_OUT)
        .iter()
        .any(|i| !mentions(i, &BY_CUSTOMER) && !mentions(i, &NOT_TEAR_OUT));
    let sealer = !named(&SEALER).is_empty();
    let phantom = items
        .iter()
        .any(|i| mentions(i, &SEAM) && mentions(i, &PHANTOM));
    let salesperson = src
        .salesperson
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_default();
    let layout = layout_from_notes(src.project_notes);

    // One area: its sq ft; several: each, as "63.5(Area #1) 9.5(Area #2)".
    let sq_ft_text = |areas: &[&Area]| -> String {
        let parts: Vec<(f64, String)> = areas
            .iter()
            .map(|a| (sq_ft(&[*a]), room_of(a)))
            .filter(|(v, _)| *v > 0.0)
            .collect();
        if parts.len() > 1 {
            parts
                .iter()
                .map(|(v, name)| format!("{}({name})", num(*v)))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            num(sq_ft(areas))
        }
    };
    let notes_for = |job_sq_ft: &str| -> String {
        let mut lines: Vec<String> = if pick_up {
            vec![
                format!("10-year sealer:{}", yes_no(sealer)),
                format!("Backsplash:{backsplash}"),
                "Additional notes:".into(),
                RULE.into(),
                format!("Salesperson:{salesperson}"),
                format!("Estimated sqft:{job_sq_ft}"),
            ]
        } else {
            vec![
                "Floor:".into(),
                format!("Backsplash:{backsplash}"),
                format!("Waterfall:{}", yes_no(waterfall)),
                format!("Stove:{stove}"),
                format!("Supports/corbels:{supports}"),
                format!("Faucet:{}", list(&faucet)),
                format!("Soap dispenser:{}", list(&soap)),
                format!("Cutting board:{}", list(&board)),
                String::new(),
                "__________________".into(),
                "Fabrication team instructions".into(),
                format!("Seam:{}", if phantom { "Phantom" } else { "STD" }),
                "Additional notes:".into(),
                RULE.into(),
                "Installation team Instructions/notes:".into(),
                format!("Tear out:{}", yes_no(tear_out)),
                format!("10 year sealer:{}", yes_no(sealer)),
                RULE.into(),
                format!("Salesperson:{salesperson}"),
                format!("Estimated sqft:{job_sq_ft}"),
                format!("Layout:{}", layout.unwrap_or_default()),
                RULE.into(),
                "Notes from the technician:".into(),
                String::new(),
                "Checked by PM: ".into(),
            ]
        };
        if pick_up {
            if let Some(layout) = layout {
                lines.push(format!("Layout:{layout}"));
            }
            lines.push(String::new());
        }
        lines.push(String::new());
        lines.join("\n")
    };

    let materials_for = |areas: &[&Area]| -> Vec<Material> {
        let mut out: Vec<Material> = Vec::new();
        for a in areas {
            let Some(pick) = option.and_then(|o| o.by_area.get(&a.id)) else {
                continue;
            };
            if pick.color.trim().is_empty() || CABINET.is_match(&pick.product) {
                continue;
            }
            let m = Material {
                brand: pick.product.trim().to_string(),
                color: pick.color.trim().to_string(),
                edge: known_edge(
                    pick.edges.first().map_or("", String::as_str),
                    src.edge_names,
                ),
            };
            if !out.contains(&m) {
                out.push(m);
            }
        }
        out
    };
    // Each sink once with its count ("CP x2"); the room after it when the
    // sinks are in several named rooms ("CP (Kitchen)"), as the office writes.
    let cutouts_for = |areas: &[&Area]| -> Vec<Cutout> {
        let mut found: Vec<(String, String, i64)> = Vec::new();
        for counter in &doc.counters {
            let Some(area) = areas.iter().find(|a| a.id == counter.area_id) else {
                continue;
            };
            let room = room_of(area);
            let room = if GENERIC_AREA.is_match(&room) {
                String::new()
            } else {
                room
            };
            for cut in counter.runs.iter().flat_map(|r| &r.cutouts) {
                let given = cut.sink_model.as_deref().filter(|m| !m.trim().is_empty());
                // "Customer Provides a sink - custom cutout" as an other cutout.
                let customer_sink = cut.kind == CutoutKind::Other
                    && given.is_none()
                    && CUSTOMER_SINK.is_match(&cut.name);
                if !(cut.kind == CutoutKind::Sink
                    || (cut.kind == CutoutKind::Other && given.is_some())
                    || customer_sink)
                {
                    continue;
                }
                let model = if customer_sink {
                    "CP".to_string()
                } else {
                    sink_model(given, cut.sink_type, cut.width)
                };
                let count = area.quantity.max(1);
                match found.iter_mut().find(|(m, r, _)| *m == model && *r == room) {
                    Some(f) => f.2 += count,
                    None => found.push((model, room.clone(), count)),
                }
            }
        }
        let mut rooms: Vec<&str> = found
            .iter()
            .map(|f| f.1.as_str())
            .filter(|r| !r.is_empty())
            .collect();
        rooms.sort_unstable();
        rooms.dedup();
        let by_room = rooms.len() > 1;
        found
            .into_iter()
            .map(|(model, room, quantity)| {
                let mut model = model;
                if quantity > 1 {
                    model = format!("{model} x{quantity}");
                }
                if by_room && !room.is_empty() {
                    model = format!("{model} ({room})");
                }
                Cutout { model, quantity }
            })
            .collect()
    };

    // A builder's job is "Company (street)", as the office names them; a
    // person's is "Last First Room". A project title that says more than the
    // customer's name names the job instead: a builder's as it is
    // ("Northpoint Management - unit 3046"), a person's "Last First" first
    // ("Drew Fairchild Shower" → "Fairchild Drew Shower").
    let address = split_address(src.address);
    let business = src.company_name.map(str::trim).filter(|c| !c.is_empty());
    let named_as_is =
        BUSINESS.is_match(src.customer_name) || ACRONYM_FIRST.is_match(src.customer_name.trim());
    let titled: Option<String> = own_title(src.project_title, src.customer_name).map(|title| {
        match after_name(title, src.customer_name) {
            Some(rest) if business.is_none() && !named_as_is => {
                format!("{} {rest}", last_name_first(src.customer_name))
                    .trim()
                    .to_string()
            }
            _ => title.to_string(),
        }
    });
    let base = match (&titled, business) {
        (Some(title), _) => title.clone(),
        (None, Some(company)) if !address.street.is_empty() => {
            format!("{company} ({})", address.street)
        }
        (None, Some(company)) => company.to_string(),
        (None, None) if named_as_is => src.customer_name.trim().to_string(),
        (None, None) => last_name_first(src.customer_name),
    };
    let job = |room: &str, areas: &[&Area]| {
        let job_sq_ft = sq_ft(areas);
        let room = match &titled {
            // The title names the room; only a pick-up or the FHBS is added.
            Some(title) if room == "Pick up" && PICK_UP.is_match(title) => "",
            Some(_) if room != "Pick up" && room != "FHBS" => "",
            _ if business.is_some() && room == "Kitchen" => "",
            _ => room,
        };
        Job {
            name: format!("{base} {room}").trim().to_string(),
            sq_ft: job_sq_ft,
            materials: materials_for(areas),
            cutouts: cutouts_for(areas),
            notes: notes_for(&sq_ft_text(areas)),
        }
    };
    let mut jobs = Vec::new();
    if !main.is_empty() || full_height.is_empty() {
        // The rooms the areas name ("Vanity", "Kitchen/Master bath"); else Kitchen.
        let mut rooms: Vec<String> = Vec::new();
        for a in &main {
            let room = room_of(a);
            if !room.is_empty() && !GENERIC_AREA.is_match(&room) && !rooms.contains(&room) {
                rooms.push(room);
            }
        }
        let room = if pick_up {
            "Pick up".to_string()
        } else if rooms.is_empty() {
            "Kitchen".to_string()
        } else {
            rooms.join("/")
        };
        jobs.push(job(&room, &main));
    }
    if !full_height.is_empty() {
        jobs.push(job("FHBS", &full_height));
    }

    Plan {
        customer_name: src.customer_name.trim().to_string(),
        phone: src.phone.map(str::trim).unwrap_or_default().to_string(),
        address,
        jobs,
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn counter(id: &str, area: &str, length: f64, splash: f64, cutouts: &Value) -> Value {
        let back = if splash > 0.0 {
            json!({"type": "splash", "splashHeight": splash})
        } else {
            json!({"type": "unfinished"})
        };
        json!({
            "id": id, "areaId": area, "x": 0, "y": 0,
            "runs": [{"length": length, "depth": 25.5, "cutouts": cutouts}],
            "edges": [back, {"type": "finished"}, {"type": "finished"}, {"type": "finished"}],
            "corners": [[null, null], [null, null]],
        })
    }

    fn sink(id: &str, model: Option<&str>, width: f64, kind: &str) -> Value {
        json!({"id": id, "kind": "sink", "width": width, "depth": 18, "sinkModel": model, "sinkType": kind})
    }

    fn doc(extra: &Value) -> QuoteDoc {
        let mut base = json!({
            "version": 2,
            "areas": [{"id": "a1", "name": "Area #1", "quantity": 1}],
            "colorOptions": [{"id": "option-1", "byArea": {"a1": {"product": "Quartz", "color": "Bianco Absoluto", "edges": ["Flat"]}}}],
            "items": [],
        });
        for (k, v) in extra.as_object().expect("object") {
            base[k] = v.clone();
        }
        QuoteDoc::parse(&base).expect("doc")
    }

    fn run(d: &QuoteDoc, notes: &[String], salesperson: &str) -> Plan {
        plan(&Source {
            customer_name: "Jane Doe",
            company_name: None,
            phone: Some("317-555-0100"),
            address: Some("1 Main St, Indianapolis, IN 46237"),
            salesperson: Some(salesperson),
            doc: d,
            option_index: 0,
            project_notes: notes,
            sq_ft_up: false,
            project_title: None,
            event_titles: &[],
            edge_names: &[],
        })
    }

    fn line<'a>(notes: &'a str, label: &str) -> Option<&'a str> {
        notes.lines().find(|l| l.starts_with(&format!("{label}:")))
    }

    #[test]
    fn names_and_addresses() {
        assert_eq!(last_name_first("Mike Pendley"), "Pendley Mike");
        assert_eq!(
            last_name_first("Marcus and Mary ARMSTRONG"),
            "ARMSTRONG Marcus and Mary"
        );
        let a = split_address(Some("8435 Smithfield Lane, Indianapolis, IN, 46237"));
        assert_eq!(
            (
                a.street.as_str(),
                a.city.as_str(),
                a.state.as_str(),
                a.zip.as_str()
            ),
            ("8435 Smithfield Lane", "Indianapolis", "IN", "46237")
        );
        let a = split_address(Some("6418 Woods Edge Drive, Spiceland, IN 47385"));
        assert_eq!(
            (a.city.as_str(), a.state.as_str(), a.zip.as_str()),
            ("Spiceland", "IN", "47385")
        );
        let a = split_address(Some("11105 Wood Ct, Carmel, IN"));
        assert_eq!(
            (a.street.as_str(), a.city.as_str(), a.zip.as_str()),
            ("11105 Wood Ct", "Carmel", "")
        );
        let a = split_address(Some("Lafayette 47905"));
        assert_eq!(
            (a.street.as_str(), a.city.as_str(), a.zip.as_str()),
            ("", "Lafayette", "47905")
        );
    }

    #[test]
    fn layout_is_the_last_one_said() {
        assert_eq!(
            layout_from_notes(&["Layout yes\nCheck".into()]),
            Some("Yes")
        );
        assert_eq!(
            layout_from_notes(&["Layout: yes".into(), "layout no".into()]),
            Some("no")
        );
        assert_eq!(layout_from_notes(&["Install by Dec 16".into()]), None);
    }

    #[test]
    fn fills_the_office_template() {
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 120.0, 4.0, &json!([
                sink("s1", Some("Kohler K-8199 (50/50)"), 33.0, "undermount"),
                {"id": "ct", "kind": "cooktop", "width": 30, "depth": 20},
            ]))],
            "items": [
                {"id": "i1", "kind": "priceList", "name": "Faucet", "option": "Weybridge Matte Black KKF240"},
                {"id": "i2", "kind": "priceList", "name": "Faucet holes (complimentary, unlimited)"},
                {"id": "i3", "kind": "priceList", "name": "Support corbel", "option": "Flat Corbel", "quantity": 7},
                {"id": "i4", "kind": "priceList", "name": "Laminate T/O"},
                {"id": "i5", "kind": "priceList", "name": "10 year Sealer"},
                {"id": "i6", "kind": "priceList", "name": "Seam", "option": "Seam Phantom"},
            ],
        }));
        let p = run(&d, &["Layout yes\nCheck".into()], "Liza Smith");
        assert_eq!(p.jobs.len(), 1);
        let job = &p.jobs[0];
        assert_eq!(job.name, "Doe Jane Kitchen");
        assert_eq!(
            job.materials,
            vec![Material {
                brand: "Quartz".into(),
                color: "Bianco Absoluto".into(),
                edge: "Flat".into()
            }]
        );
        assert_eq!(
            job.cutouts,
            vec![Cutout {
                model: "Kohler K-8199 (50/50)".into(),
                quantity: 1
            }]
        );
        let n = job.notes.as_str();
        assert_eq!(line(n, "Backsplash"), Some("Backsplash:4\""));
        assert_eq!(line(n, "Stove"), Some("Stove:C/T"));
        assert_eq!(line(n, "Supports/corbels"), Some("Supports/corbels:7"));
        assert_eq!(
            line(n, "Faucet"),
            Some("Faucet:Weybridge Matte Black KKF240")
        );
        assert_eq!(line(n, "Seam"), Some("Seam:Phantom"));
        assert_eq!(line(n, "Tear out"), Some("Tear out:Yes"));
        assert_eq!(line(n, "10 year sealer"), Some("10 year sealer:Yes"));
        assert_eq!(line(n, "Salesperson"), Some("Salesperson:Liza"));
        assert_eq!(line(n, "Layout"), Some("Layout:Yes"));
        assert!(n.starts_with("Floor:\n"));
        assert!(n.ends_with("Checked by PM: \n"));
        assert_eq!(p.address.street, "1 Main St");
    }

    #[test]
    fn says_no_when_the_quote_has_no_extras() {
        let d = doc(&json!({"counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))]}));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        for label in [
            "Backsplash",
            "Waterfall",
            "Faucet",
            "Soap dispenser",
            "Cutting board",
            "Tear out",
            "10 year sealer",
        ] {
            assert_eq!(
                line(&job.notes, label),
                Some(format!("{label}:no").as_str())
            );
        }
        assert_eq!(line(&job.notes, "Stove"), Some("Stove:F/S"));
        assert_eq!(line(&job.notes, "Seam"), Some("Seam:STD"));
        assert_eq!(line(&job.notes, "Layout"), Some("Layout:"));
    }

    #[test]
    fn full_height_is_its_own_job() {
        let d = doc(&json!({
            "areas": [{"id": "a1", "name": "Area #1", "quantity": 1}, {"id": "fh", "name": "Full Height", "quantity": 1}],
            "colorOptions": [{"id": "option-1", "byArea": {
                "a1": {"product": "Dolomite", "color": "Fantasy Brown 3", "edges": ["Flat"]},
                "fh": {"product": "Dolomite", "color": "Fantasy Brown 3", "edges": ["Ogee"]},
            }}],
            "counters": [
                counter("c1", "a1", 128.0, 0.0, &json!([sink("s1", None, 30.0, "farmer")])),
                counter("c2", "fh", 128.0, 0.0, &json!([])),
            ],
        }));
        let p = run(&d, &[], "Kyle");
        let names: Vec<&str> = p.jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["Doe Jane Kitchen", "Doe Jane FHBS"]);
        assert_eq!(
            p.jobs[0].cutouts,
            vec![Cutout {
                model: "30\" Farmer Sink Cutout".into(),
                quantity: 1
            }]
        );
        assert_eq!(p.jobs[1].cutouts.len(), 0);
        assert_eq!(p.jobs[1].materials[0].edge, "Ogee");
        for job in &p.jobs {
            assert_eq!(line(&job.notes, "Backsplash"), Some("Backsplash:FH"));
        }
    }

    #[test]
    fn pick_up_uses_the_short_note() {
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 60.0, 0.0, &json!([]))],
            "texts": [{"id": "t1", "x": 0, "y": 0, "text": "PICK UP"}],
        }));
        let job = run(&d, &[], "Polly").jobs.remove(0);
        assert_eq!(job.name, "Doe Jane Pick up");
        assert_eq!(
            job.notes,
            format!(
                "10-year sealer:no\nBacksplash:no\nAdditional notes:\n{RULE}\nSalesperson:Polly\nEstimated sqft:{}\n\n",
                num(job.sq_ft)
            )
        );
    }

    #[test]
    fn a_builder_job_is_named_by_company_and_street() {
        let d = doc(&json!({"counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))]}));
        let named = |company: Option<&str>, address: &str| {
            plan(&Source {
                customer_name: "Darreld Smith",
                company_name: company,
                phone: None,
                address: Some(address),
                salesperson: Some("Liza"),
                doc: &d,
                option_index: 0,
                project_notes: &[],
                sq_ft_up: false,
                project_title: None,
                event_titles: &[],
                edge_names: &[],
            })
            .jobs
            .remove(0)
            .name
        };
        assert_eq!(
            named(
                Some("Cedarbrook"),
                "4440 Crittenden Ave, Indianapolis, IN 46205"
            ),
            "Cedarbrook (4440 Crittenden Ave)"
        );
        assert_eq!(named(Some(" TBG "), "Indianapolis, IN"), "TBG");
        assert_eq!(
            named(Some(""), "1 Main St, Carmel, IN"),
            "Smith Darreld Kitchen"
        );
    }

    #[test]
    fn rules_learned_from_the_office_jobs() {
        // A business with no company name keeps its name.
        let d = doc(&json!({"counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))]}));
        let p = plan(&Source {
            customer_name: "GTP Holdings",
            company_name: None,
            phone: None,
            address: None,
            salesperson: Some("Kyle"),
            doc: &d,
            option_index: 0,
            project_notes: &[],
            sq_ft_up: false,
            project_title: None,
            event_titles: &[],
            edge_names: &[],
        });
        assert_eq!(p.jobs[0].name, "GTP Holdings Kitchen");
        // No installation is a pick-up; a tear out the customer does is not ours.
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 60.0, 0.0, &json!([]))],
            "items": [
                {"id": "i1", "kind": "misc", "name": "NO INSTALLATION"},
                {"id": "i2", "kind": "priceList", "name": "Tear Out to be completed by customer"},
            ],
        }));
        let job = run(&d, &[], "Polly").jobs.remove(0);
        assert_eq!(job.name, "Doe Jane Pick up");
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 60.0, 0.0, &json!([]))],
            "items": [{"id": "i2", "kind": "priceList", "name": "Tear Out to be completed by customer"}],
        }));
        let job = run(&d, &[], "Polly").jobs.remove(0);
        assert_eq!(line(&job.notes, "Tear out"), Some("Tear out:no"));
    }

    #[test]
    fn rooms_sq_ft_and_what_the_customer_does() {
        let d = doc(&json!({
            "areas": [{"id": "a1", "name": "Vanity", "quantity": 1}, {"id": "a2", "name": "Master bath", "quantity": 1}],
            "colorOptions": [{"id": "o1", "byArea": {
                "a1": {"product": "Quartz", "color": "Taj", "edges": ["Flat"]},
                "a2": {"product": "Cabinets", "color": "Shaker Champagne", "edges": []},
            }}],
            "counters": [counter("c1", "a1", 60.0, 0.0, &json!([])), counter("c2", "a2", 48.0, 0.0, &json!([]))],
            "items": [
                {"id": "i1", "kind": "misc", "name": "No existing countertops to tear out"},
                {"id": "i2", "kind": "priceList", "name": "Wall Tile Tear Out"},
                {"id": "i3", "kind": "priceList", "name": "Faucet", "option": "Customer provides a faucet"},
            ],
        }));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        assert_eq!(job.name, "Doe Jane Vanity/Master bath");
        assert_eq!(job.materials.len(), 1);
        assert_eq!(line(&job.notes, "Tear out"), Some("Tear out:no"));
        assert_eq!(line(&job.notes, "Faucet"), Some("Faucet:no"));
        let sq_ft = line(&job.notes, "Estimated sqft").unwrap();
        assert!(
            sq_ft.contains("(Vanity) ") && sq_ft.ends_with("(Master bath)"),
            "{sq_ft}"
        );
    }

    #[test]
    fn rules_from_the_wide_check() {
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 96.0, 4.0, &json!([
                {"id": "o1", "kind": "other", "width": 17, "depth": 13, "sinkModel": "Kohler Caxton vanity sink"},
            ]))],
            "texts": [{"id": "t1", "x": 0, "y": 0, "text": "FHBS"}],
            "items": [
                {"id": "i1", "kind": "priceList", "name": "Seam Phantom"},
                {"id": "i2", "kind": "misc", "name": "DW brackets- FREE"},
            ],
        }));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        assert_eq!(line(&job.notes, "Seam"), Some("Seam:Phantom"));
        assert_eq!(
            line(&job.notes, "Supports/corbels"),
            Some("Supports/corbels:0")
        );
        assert_eq!(line(&job.notes, "Backsplash"), Some("Backsplash:FH"));
        assert_eq!(
            job.cutouts,
            vec![Cutout {
                model: "Kohler Caxton vanity sink".into(),
                quantity: 1
            }]
        );
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 96.0, 4.0, &json!([]))],
            "texts": [{"id": "t1", "x": 0, "y": 0, "text": "no backsplash"}],
        }));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        assert_eq!(line(&job.notes, "Backsplash"), Some("Backsplash:no"));
    }

    #[test]
    fn a_pick_up_discount_is_no_pick_up() {
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))],
            "items": [{"id": "i1", "kind": "misc", "name": "Black Friday Discount (10% pick up)"}],
        }));
        assert_eq!(run(&d, &[], "Liza").jobs[0].name, "Doe Jane Kitchen");
    }

    #[test]
    fn sinks_as_the_office_writes_them() {
        let d = doc(&json!({
            "areas": [
                {"id": "a1", "name": "Kitchen", "quantity": 1},
                {"id": "a2", "name": "Master bath", "quantity": 2},
            ],
            "colorOptions": [{"id": "option-1", "byArea": {
                "a1": {"product": "Quartz", "color": "Bianco", "edges": ["Flat"]},
                "a2": {"product": "Quartz", "color": "Bianco", "edges": ["Flat"]},
            }}],
            "counters": [
                counter("c1", "a1", 96.0, 4.0, &json!([sink("s1", Some("Customer Provides a sink - Custom Cut Out"), 30.0, "undermount")])),
                counter("c2", "a2", 48.0, 4.0, &json!([sink("s2", Some("Rectangular vanity sink"), 18.0, "undermount")])),
            ],
        }));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        assert_eq!(
            job.cutouts,
            vec![
                Cutout {
                    model: "CP (Kitchen)".into(),
                    quantity: 1
                },
                Cutout {
                    model: "Rectangular vanity sink x2 (Master bath)".into(),
                    quantity: 2
                },
            ]
        );
        let d = doc(&json!({
            "counters": [counter("c1", "a1", 96.0, 4.0, &json!([
                sink("s1", Some("Customer provides"), 30.0, "undermount"),
                sink("s2", Some("Customer provides"), 30.0, "undermount"),
            ]))],
        }));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        assert_eq!(
            job.cutouts,
            vec![Cutout {
                model: "CP x2".into(),
                quantity: 2
            }]
        );
    }

    /// Like `run`, with the project's title, event titles and price list edges.
    fn titled(
        d: &QuoteDoc,
        customer: &str,
        company: Option<&str>,
        title: &str,
        events: &[String],
    ) -> Plan {
        let edges = ["Flat".to_string(), "Eased".to_string()];
        plan(&Source {
            customer_name: customer,
            company_name: company,
            phone: None,
            address: Some("1 Main St, Indianapolis, IN 46237"),
            salesperson: Some("Liza"),
            doc: d,
            option_index: 0,
            project_notes: &[],
            sq_ft_up: false,
            project_title: Some(title),
            event_titles: events,
            edge_names: &edges,
        })
    }

    #[test]
    fn layout_as_the_office_writes_it() {
        assert_eq!(layout_from_notes(&["L/O YES".into()]), Some("Yes"));
        assert_eq!(layout_from_notes(&["Layout - no".into()]), Some("no"));
        assert_eq!(layout_from_notes(&["Layout: Yes".into()]), Some("Yes"));
    }

    #[test]
    fn a_pick_up_event_makes_a_pick_up() {
        let d = doc(&json!({"counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))]}));
        let p = titled(
            &d,
            "Deborah Lashbrook",
            None,
            "Deborah Lashbrook",
            &["Deborah Lashbrook - Pick up".into()],
        );
        assert_eq!(p.jobs[0].name, "Lashbrook Deborah Pick up");
        assert!(
            p.jobs[0].notes.starts_with("10-year sealer:"),
            "{}",
            p.jobs[0].notes
        );
        let p = titled(
            &d,
            "April Deloney",
            None,
            "April Deloney",
            &["Cabinet Pick up - April Deloney".into()],
        );
        assert_eq!(p.jobs[0].name, "Deloney April Kitchen");
    }

    #[test]
    fn an_other_cutout_the_customer_provides_is_cp() {
        let d = doc(&json!({"counters": [counter("c1", "a1", 96.0, 4.0, &json!([
            {"id": "o1", "kind": "other", "width": 30, "depth": 18, "sinkModel": null,
             "name": "Customer Provides a sink - custom cutout"},
            {"id": "o2", "kind": "other", "width": 4, "depth": 4, "name": "Outlet"},
        ]))]}));
        let job = run(&d, &[], "Liza").jobs.remove(0);
        assert_eq!(
            job.cutouts,
            vec![Cutout {
                model: "CP".into(),
                quantity: 1
            }]
        );
    }

    #[test]
    fn an_edge_the_price_list_has_not_is_its_default() {
        let d = doc(&json!({
            "colorOptions": [{"id": "option-1", "byArea": {"a1": {"product": "Quartz", "color": "Frost White", "edges": ["55"]}}}],
            "counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))],
        }));
        assert_eq!(
            titled(&d, "Jane Doe", None, "", &[]).jobs[0].materials[0].edge,
            "Flat"
        );
        let d = doc(&json!({
            "colorOptions": [{"id": "option-1", "byArea": {"a1": {"product": "Quartz", "color": "Frost White", "edges": ["eased"]}}}],
            "counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))],
        }));
        assert_eq!(
            titled(&d, "Jane Doe", None, "", &[]).jobs[0].materials[0].edge,
            "Eased"
        );
        let d = doc(&json!({
            "colorOptions": [{"id": "option-1", "byArea": {"a1": {"product": "Quartz", "color": "Frost White", "edges": ["Mitered 2in"]}}}],
            "counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))],
        }));
        assert_eq!(
            titled(&d, "Jane Doe", None, "", &[]).jobs[0].materials[0].edge,
            "Mitered 2in"
        );
    }

    #[test]
    fn a_project_title_names_the_job() {
        let d = doc(&json!({
            "areas": [{"id": "a1", "name": "Unit 3046", "quantity": 1}],
            "colorOptions": [{"id": "option-1", "byArea": {"a1": {"product": "Granite", "color": "Luna Pearl", "edges": ["Flat"]}}}],
            "counters": [counter("c1", "a1", 96.0, 4.0, &json!([]))],
        }));
        let name = |customer: &str, company: Option<&str>, title: &str| {
            titled(&d, customer, company, title, &[])
                .jobs
                .remove(0)
                .name
        };
        assert_eq!(
            name(
                "Rachel Howard",
                Some("Northpoint Management"),
                "Northpoint Management - unit 3046"
            ),
            "Northpoint Management - unit 3046"
        );
        assert_eq!(
            name(
                "Jayde Hernandez",
                Some("JTC Enterprises, Inc."),
                "Jayde Hernandez - N 175 W"
            ),
            "Jayde Hernandez - N 175 W"
        );
        assert_eq!(
            name("Drew Fairchild", None, "Drew Fairchild Shower"),
            "Fairchild Drew Shower"
        );
        assert_eq!(
            name("Drew Fairchild", None, "Drew Fairchildson"),
            "Drew Fairchildson"
        );
        // The customer's name alone, or "… project", says nothing.
        assert_eq!(
            name("Brad Wilson", None, "brad  wilson"),
            "Wilson Brad Unit 3046"
        );
        assert_eq!(
            name("Brad Wilson", None, "Brad Wilson project"),
            "Wilson Brad Unit 3046"
        );
        assert_eq!(
            name("Darreld Smith", Some("TBG"), ""),
            "TBG (1 Main St) Unit 3046"
        );
        // A pick-up title is not said twice; another gets "Pick up".
        let pick = ["I - Lashbrook - Pick up".to_string()];
        assert_eq!(
            titled(
                &d,
                "Deborah Lashbrook",
                None,
                "Deborah Lashbrook Pick up",
                &pick
            )
            .jobs[0]
                .name,
            "Lashbrook Deborah Pick up"
        );
        assert_eq!(
            titled(
                &d,
                "Deborah Lashbrook",
                None,
                "Deborah Lashbrook Bar",
                &pick
            )
            .jobs[0]
                .name,
            "Lashbrook Deborah Bar Pick up"
        );
    }

    #[test]
    fn a_builder_short_name_stays_first() {
        let d = doc(&json!({"counters": [counter("c1", "a1", 96.0, 0.0, &json!([]))]}));
        assert_eq!(
            titled(&d, "IHDC Kim Keyser", None, "IHDC Kim Keyser", &[]).jobs[0].name,
            "IHDC Kim Keyser Kitchen"
        );
        assert_eq!(
            titled(&d, "JOHN SMITH", None, "", &[]).jobs[0].name,
            "SMITH JOHN Kitchen"
        );
    }

    #[test]
    fn one_unnamed_area_is_the_kitchen() {
        let d = doc(&json!({
            "areas": [{"id": "a1", "name": "Area #1", "quantity": 1}, {"id": "a2", "name": "Vanity", "quantity": 1}],
            "colorOptions": [{"id": "option-1", "byArea": {
                "a1": {"product": "Quartz", "color": "Sparkling White", "edges": ["Eased"]},
                "a2": {"product": "Quartz", "color": "Sparkling White", "edges": ["Eased"]},
            }}],
            "counters": [counter("c1", "a1", 120.0, 4.0, &json!([])), counter("c2", "a2", 60.0, 4.0, &json!([]))],
        }));
        let job = run(&d, &[], "Kyle").jobs.remove(0);
        assert_eq!(job.name, "Doe Jane Kitchen/Vanity");
        let sq_ft = line(&job.notes, "Estimated sqft").unwrap();
        assert!(
            sq_ft.contains("(Kitchen) ") && sq_ft.ends_with("(Vanity)"),
            "{sq_ft}"
        );
        // Two unnamed areas keep their names.
        let d = doc(&json!({
            "areas": [{"id": "a1", "name": "Area #1", "quantity": 1}, {"id": "a2", "name": "Area #2", "quantity": 1}],
            "colorOptions": [{"id": "option-1", "byArea": {
                "a1": {"product": "Quartz", "color": "Bianco", "edges": ["Flat"]},
                "a2": {"product": "Quartz", "color": "Bianco", "edges": ["Flat"]},
            }}],
            "counters": [counter("c1", "a1", 120.0, 4.0, &json!([])), counter("c2", "a2", 60.0, 4.0, &json!([]))],
        }));
        let job = run(&d, &[], "Kyle").jobs.remove(0);
        assert_eq!(job.name, "Doe Jane Kitchen");
        assert!(
            line(&job.notes, "Estimated sqft")
                .unwrap()
                .ends_with("(Area #2)")
        );
    }
}
