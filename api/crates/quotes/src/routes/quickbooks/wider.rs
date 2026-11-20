//! What a company keeps doing to its invoice drafts that no single draft
//! line shows.
//!
//! [`super::rules`] learns what becomes of one line: "33.5 lin ft Finished
//! Edge - Flat" is reworded, "Laser Template" is left out. Two habits are
//! wider than that:
//!
//! - **The same rewording on every line that starts alike.** An office that
//!   drops the count and unit a line starts with ("103.2 sq ft Quartz Taj
//!   Supreme" saved as "Quartz Taj Supreme") does it to each color, and each
//!   color is a line of its own that may never come three times. [`Leads`]
//!   learns the start ("# sq ft") instead: once the lines that start so keep
//!   losing it, a line seen for the first time loses it too.
//! - **Lines typed in.** A text line the rep adds is no draft line, so it
//!   has no outcome. It is kept apart (`field = 'added'`, [`added_lines`])
//!   with the line it was put after, every invoice says whether it got one
//!   (a `none` row when nothing was typed in), and a text the company's
//!   invoices keep getting is added to the draft ([`added_history`],
//!   [`insert`]), always or on the quotes it goes with, by the very rules a
//!   draft line follows ([`super::rules::line_rules`]).
//!
//! Like every learned rule they never change an amount, the rep can undo
//! them in the editor, and undoing them twice turns them off.

use super::invoice::Line;
use super::rules::{LinePast, MIN_SEEN, Outcome, Sizes, WHEN_WINDOW, WINDOW, fill, shape};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Most words a line's start has: a count and its unit ("# lin ft").
const LEAD_WORDS: usize = 4;
/// `draft_shape` holds this many characters.
const SHAPE_CHARS: usize = 255;
/// `draft_text` holds this many.
const TEXT_CHARS: usize = 1000;
/// Most lines added to one draft.
const MOST_ADDED: usize = 12;

/// A shape as it is kept.
pub(super) fn short(text: &str) -> String {
    shape(text).chars().take(SHAPE_CHARS).collect()
}

/// A word, not a dash or other mark between words.
fn is_word(token: &str) -> bool {
    token.chars().any(|c| c.is_alphanumeric() || c == '#')
}

fn is_mark(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '–' | '—' | ':' | ',')
}

/// The first `words` words of a line that starts with a count, as a shape
/// ("# sq ft"), and what follows them, without the dash between: ("# sq ft",
/// "4\" Splash") for "7.3 sq ft - 4\" Splash" and 3 words. None when the
/// line does not start with a count or nothing follows.
fn after_lead(text: &str, words: usize) -> Option<(String, &str)> {
    let text = text.trim();
    let mut end = None;
    let mut seen = 0;
    for token in text.split_whitespace().filter(|token| is_word(token)) {
        seen += 1;
        if seen == words {
            // A subslice of `text`: where it ends in it.
            end = Some(token.as_ptr() as usize - text.as_ptr() as usize + token.len());
            break;
        }
    }
    let end = end?;
    let rest = text[end..].trim_start_matches(is_mark).trim_end();
    if !rest.chars().any(char::is_alphanumeric) {
        return None;
    }
    let lead = shape(&text[..end])
        .split_whitespace()
        .filter(|token| is_word(token))
        .collect::<Vec<_>>()
        .join(" ");
    lead.starts_with('#').then_some((lead, rest))
}

/// The start a draft line lost when it was saved as `saved`: "# sq ft" for
/// "103.2 sq ft Quartz Taj Supreme" saved as "Quartz Taj Supreme".
fn lost_lead(draft: &str, saved: &str) -> Option<String> {
    (1..=LEAD_WORDS).find_map(|words| {
        after_lead(draft, words)
            .filter(|(_, rest)| rest.eq_ignore_ascii_case(saved.trim()))
            .map(|(lead, _)| lead)
    })
}

/// One kept outcome of a draft line.
#[derive(Debug, Clone, Copy)]
pub(super) struct LeadPast<'a> {
    pub shape: &'a str,
    /// `kept`, `changed` or `removed`.
    pub outcome: &'a str,
    /// What it was changed to (a template).
    pub value: Option<&'a str>,
    pub draft_text: &'a str,
}

/// The starts ("# sq ft", "# lin ft") the company's lines keep losing.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Leads {
    /// Every start a line ever lost, longest first.
    known: Vec<String>,
    /// Those the lines that start so keep losing.
    ruled: HashSet<String>,
    /// Shapes whose own line last stayed as it was, or was reworded another
    /// way: they follow their own outcomes.
    contrary: HashSet<String>,
}

/// The longest known start a shape begins with, with more after it.
fn lead_of<'a>(known: &'a [String], shape: &str) -> Option<&'a String> {
    let words: Vec<&str> = shape
        .split_whitespace()
        .filter(|token| is_word(token))
        .collect();
    known.iter().find(|lead| {
        let lead: Vec<&str> = lead.split_whitespace().collect();
        words.len() > lead.len() && words[..lead.len()] == lead[..]
    })
}

impl Leads {
    /// Learned from the outcomes of the company's draft lines, newest
    /// first. Each line (shape) counts once, by what last became of it
    /// (leaving a line out says nothing of its wording): a start is dropped
    /// from new lines once at least [`MIN_SEEN`] of the latest [`WINDOW`]
    /// lines that start so lost it, 4 of 5 of them, unless the 2 newest
    /// both kept it.
    pub fn learn(past: &[LeadPast<'_>]) -> Self {
        let mut known = BTreeSet::new();
        let mut seen = HashSet::new();
        let mut latest: Vec<(&str, Option<String>)> = Vec::new();
        for row in past.iter().filter(|row| row.outcome != "removed") {
            let lost = row
                .value
                .filter(|_| row.outcome == "changed")
                .and_then(|to| fill(to, row.draft_text))
                .and_then(|saved| lost_lead(row.draft_text, &saved));
            if let Some(lead) = &lost {
                known.insert(lead.clone());
            }
            if seen.insert(row.shape) {
                latest.push((row.shape, lost));
            }
        }
        let mut known: Vec<String> = known.into_iter().collect();
        known.sort_by_key(|lead| std::cmp::Reverse(lead.split_whitespace().count()));
        let mut lines: HashMap<&String, Vec<bool>> = HashMap::new();
        let mut contrary = HashSet::new();
        for (shape, lost) in &latest {
            let Some(lead) = lead_of(&known, shape) else {
                continue;
            };
            let lost = lost.as_ref() == Some(lead);
            if !lost {
                contrary.insert((*shape).to_string());
            }
            let lines = lines.entry(lead).or_default();
            if lines.len() < WINDOW {
                lines.push(lost);
            }
        }
        let ruled = lines
            .into_iter()
            .filter(|(_, lines)| {
                let lost = lines.iter().filter(|lost| **lost).count();
                lost >= MIN_SEEN
                    && lost * 5 >= lines.len() * 4
                    && lines.iter().take(2).any(|lost| *lost)
            })
            .map(|(lead, _)| lead.clone())
            .collect();
        Self {
            known,
            ruled,
            contrary,
        }
    }

    /// The line without the start the company's lines keep losing; None for
    /// a line that keeps its text.
    pub fn strip(&self, text: &str) -> Option<String> {
        let shape = shape(text);
        if self.contrary.contains(&shape) {
            return None;
        }
        let lead = lead_of(&self.known, &shape).filter(|lead| self.ruled.contains(*lead))?;
        let (found, rest) = after_lead(text, lead.split_whitespace().count())?;
        (found == *lead).then(|| rest.to_string())
    }
}

fn is_text(line: &Line) -> bool {
    line.detail_type == "DescriptionOnly"
}

fn said(line: &Line) -> &str {
    line.description.as_deref().map_or("", str::trim)
}

/// A text line typed into an invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedLine {
    pub shape: String,
    pub text: String,
    /// The line it was put after: a draft line's shape, `+` and the shape of
    /// another line typed in, or empty for the top of the invoice.
    pub after: String,
    /// Its id in the editor.
    pub line_id: Option<String>,
}

/// Where a line typed in stands: after `before`, a draft line (by its text
/// in the draft) or another line typed in.
pub(super) fn after(before: Option<(&str, bool)>) -> String {
    match before {
        None => String::new(),
        Some((drafted, true)) => short(drafted),
        Some((typed, false)) => format!("+{}", short(typed)),
    }
}

/// The text lines of the saved invoice that were no line of the draft (as
/// the editor opened it, before rules): typed in by the rep, or added by a
/// rule and kept.
pub fn added_lines(draft: &[Line], saved: &[Line]) -> Vec<AddedLine> {
    let drafted: HashMap<&str, &str> = draft
        .iter()
        .filter_map(|line| Some((line.id.as_deref()?, said(line))))
        .collect();
    let mut added = Vec::new();
    let mut before: Option<(&str, bool)> = None;
    for line in saved.iter().filter(|line| !said(line).is_empty()) {
        let own = line.id.as_deref().and_then(|id| drafted.get(id).copied());
        if own.is_none() && is_text(line) && !is_told(line) {
            added.push(AddedLine {
                shape: short(said(line)),
                text: said(line).chars().take(TEXT_CHARS).collect(),
                after: after(before),
                line_id: line.id.clone(),
            });
        }
        before = Some(own.map_or((said(line), false), |drafted| (drafted, true)));
    }
    added
}

/// What the editor showed for a draft, the company's way: what the rep
/// then saved was made by hand where it differs, and only left as it was
/// where it does not.
#[derive(Debug, Default)]
pub struct Shown {
    /// Each draft line's text as shown, by id; none for a line left out.
    lines: HashMap<String, Option<String>>,
    /// The texts added to the draft ([`insert`]).
    added: Vec<String>,
    /// Off when what was shown could not be worked out.
    known: bool,
}

impl Shown {
    /// From the draft as the quote made it and what the rules made of it:
    /// the lines shown and the ones left out.
    pub(super) fn of(draft: &[Line], lines: &[Line], removed: &[Line]) -> Self {
        let drafted: HashSet<&str> = draft.iter().filter_map(|line| line.id.as_deref()).collect();
        let own = |line: &&Line| line.id.as_deref().is_some_and(|id| drafted.contains(id));
        let id = |line: &Line| line.id.clone().unwrap_or_default();
        Self {
            lines: lines
                .iter()
                .filter(own)
                .map(|line| (id(line), Some(said(line).to_string())))
                .chain(removed.iter().filter(own).map(|line| (id(line), None)))
                .collect(),
            added: lines
                .iter()
                .filter(|line| is_usual(line))
                .map(|line| said(line).to_string())
                .collect(),
            known: true,
        }
    }

    /// Whether the rep made a draft line what it was saved as (`saved`: its
    /// text, none when it is not on the invoice) by hand.
    pub fn by_hand(&self, id: &str, saved: Option<&str>) -> Option<bool> {
        let saved = saved.map(str::trim).filter(|saved| !saved.is_empty());
        let shown = self.lines.get(id).filter(|_| self.known)?;
        Some(shown.as_deref() != saved)
    }

    /// Whether the rep typed a line that is no draft line (else a rule
    /// added it and they left it in).
    pub fn typed(&self, line: &AddedLine) -> Option<bool> {
        self.known
            .then(|| !self.added.iter().any(|text| short(text) == line.shape))
    }

    /// The lines a rule added that are not among the ones saved (`kept`):
    /// the rep deleted them.
    pub fn deleted(&self, kept: &[AddedLine]) -> Vec<AddedLine> {
        self.added
            .iter()
            .filter(|text| !kept.iter().any(|line| line.shape == short(text)))
            .map(|text| AddedLine {
                shape: short(text),
                text: text.clone(),
                after: String::new(),
                line_id: None,
            })
            .collect()
    }
}

/// One kept row of what was typed into an invoice (`field = 'added'`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct AddedRow {
    pub id: i32,
    pub qbo_invoice_id: String,
    /// `added`; `none` for an invoice nothing was typed into; `removed` for
    /// a line a rule added that the rep deleted, or one deleted since in
    /// `QuickBooks`.
    pub outcome: String,
    pub draft_shape: Option<String>,
    pub draft_text: Option<String>,
    /// [`AddedLine::after`].
    pub saved_value: Option<String>,
    /// The rep typed it (or deleted it); not so for a line a rule added
    /// that they left in.
    pub by_hand: Option<bool>,
}

/// A text the company's invoices keep getting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Usual {
    pub text: String,
    /// [`AddedLine::after`].
    pub after: String,
    /// Its place among the lines typed in.
    order: i32,
}

/// What was typed into the company's invoices, as line histories: for each
/// text (by shape), whether each of the latest invoices got it (`added`) or
/// not (`absent`), newest invoice first, with the words and sizes of its
/// quote; and the text itself, as it was last typed. `rows` are newest
/// invoice first. A text with a count in it that differs from one invoice
/// to the next ("Measured 34.39 sqft") cannot be written for the rep and is
/// left out.
pub(super) fn added_history(
    rows: &[AddedRow],
    words: &HashMap<String, Vec<String>>,
    sizes: &HashMap<String, Arc<Sizes>>,
) -> (HashMap<String, Vec<LinePast>>, HashMap<String, Usual>) {
    let mut invoices: Vec<&str> = Vec::new();
    // By invoice and shape: whether it got the text, and by the rep's hand.
    let mut got: HashMap<&str, HashMap<&str, (bool, bool)>> = HashMap::new();
    let mut texts: HashMap<&str, Vec<&AddedRow>> = HashMap::new();
    for row in rows {
        let invoice = row.qbo_invoice_id.as_str();
        if !got.contains_key(invoice) {
            if invoices.len() == WHEN_WINDOW {
                continue;
            }
            invoices.push(invoice);
            got.insert(invoice, HashMap::new());
        }
        let Some(shape) = row.draft_shape.as_deref() else {
            continue;
        };
        let of = got.entry(invoice).or_default();
        match row.outcome.as_str() {
            "added" => {
                of.insert(shape, (true, row.by_hand.unwrap_or(true)));
                texts.entry(shape).or_default().push(row);
            }
            // Deleted by the rep: they did not want it there.
            "removed" => {
                of.entry(shape).or_insert((false, true));
            }
            _ => {}
        }
    }
    let mut history = HashMap::new();
    let mut usual = HashMap::new();
    for (shape, typed) in texts {
        let text = |row: &AddedRow| row.draft_text.as_deref().map_or("", str::trim).to_string();
        let newest = text(typed[0]);
        let varies = shape.contains('#')
            && typed
                .get(1)
                .is_none_or(|older| !text(older).eq_ignore_ascii_case(&newest));
        if newest.is_empty() || varies {
            continue;
        }
        let past = invoices
            .iter()
            .map(|invoice| {
                // No row: nothing was shown and nothing typed.
                let (added, by_hand) = got[invoice].get(shape).copied().unwrap_or((false, false));
                LinePast {
                    outcome: Outcome::new(if added { "added" } else { "absent" }, None),
                    words: words.get(*invoice).cloned(),
                    sizes: sizes.get(*invoice).cloned().unwrap_or_default(),
                    by_hand,
                }
            })
            .collect();
        history.insert(shape.to_string(), past);
        usual.insert(
            shape.to_string(),
            Usual {
                text: newest,
                after: typed[0].saved_value.clone().unwrap_or_default(),
                order: typed[0].id,
            },
        );
    }
    (history, usual)
}

/// A line an admin's instruction added: no habit of the reps.
fn is_told(line: &Line) -> bool {
    line.id
        .as_deref()
        .is_some_and(|id| id.starts_with("new:told:"))
}

/// The id of a line a rule added.
fn is_usual(line: &Line) -> bool {
    line.id
        .as_deref()
        .is_some_and(|id| id.starts_with("new:usual-"))
}

/// Adds the texts the company's invoices keep getting to a draft, each
/// after the line it is usually put after (`drafted`: each draft line's
/// shape before rules, by id), else at the end; a text the draft already
/// has is not added again.
#[allow(clippy::implicit_hasher)]
pub(super) fn insert(
    lines: &mut Vec<Line>,
    drafted: &HashMap<String, String>,
    mut usual: Vec<Usual>,
) {
    usual.sort_by_key(|usual| usual.order);
    usual.truncate(MOST_ADDED);
    let mut waiting: Vec<(usize, Usual)> = usual.into_iter().enumerate().collect();
    // A line put after another added line waits for it.
    while !waiting.is_empty() {
        let before = waiting.len();
        let mut later = Vec::new();
        for (n, usual) in waiting {
            let shape = short(&usual.text);
            if lines.iter().any(|line| short(said(line)) == shape) {
                continue;
            }
            let at = if usual.after.is_empty() {
                Some(0)
            } else if let Some(typed) = usual.after.strip_prefix('+') {
                lines
                    .iter()
                    .rposition(|line| is_usual(line) && short(said(line)) == typed)
                    .map(|at| at + 1)
            } else {
                lines
                    .iter()
                    .rposition(|line| {
                        line.id.as_deref().and_then(|id| drafted.get(id)) == Some(&usual.after)
                    })
                    .map(|at| at + 1)
            };
            let Some(mut at) = at else {
                later.push((n, usual));
                continue;
            };
            // After the lines already added there, in the order typed.
            while lines.get(at).is_some_and(is_usual) && !usual.after.is_empty() {
                at += 1;
            }
            lines.insert(at, added(n, usual.text));
        }
        if later.len() == before {
            // The lines they followed are not on this quote: at the end.
            for (n, usual) in later {
                lines.push(added(n, usual.text));
            }
            break;
        }
        waiting = later;
    }
}

fn added(n: usize, text: String) -> Line {
    Line {
        id: Some(format!("new:usual-{n}")),
        line_num: None,
        detail_type: "DescriptionOnly".into(),
        description: Some(text),
        amount: None,
        qty: None,
        unit_price: None,
        item_ref_value: None,
        item_ref_name: None,
        editable: true,
        class_name: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(id: &str, description: &str) -> Line {
        Line {
            id: Some(id.into()),
            ..added(0, description.into())
        }
    }

    fn changed<'a>(shape: &'a str, draft: &'a str, to: &'a str) -> LeadPast<'a> {
        LeadPast {
            shape,
            outcome: "changed",
            value: Some(to),
            draft_text: draft,
        }
    }

    fn kept<'a>(shape: &'a str, draft: &'a str) -> LeadPast<'a> {
        LeadPast {
            shape,
            outcome: "kept",
            value: None,
            draft_text: draft,
        }
    }

    #[test]
    fn a_line_start_is_its_count_and_unit_without_the_dash() {
        assert_eq!(
            after_lead("103.2 sq ft Quartz Taj Supreme", 3),
            Some(("# sq ft".to_string(), "Quartz Taj Supreme"))
        );
        assert_eq!(
            after_lead("7.3 sq ft - 4\" Splash", 3),
            Some(("# sq ft".to_string(), "4\" Splash"))
        );
        assert_eq!(after_lead("Laser Template", 1), None);
        assert_eq!(after_lead("12 lin ft", 3), None);
        assert_eq!(
            lost_lead("28.3 lin ft Finished Edge - Flat", "finished edge - flat"),
            Some("# lin ft".to_string())
        );
        assert_eq!(
            lost_lead("28.3 lin ft Finished Edge - Flat", "Flat Edge"),
            None
        );
    }

    #[test]
    fn a_start_most_lines_lose_is_dropped_from_a_line_never_seen() {
        let shapes: Vec<String> = ["Quartz Taj", "Granite Luna", "Quartz Frost", "- 4\" Splash"]
            .iter()
            .map(|rest| shape(&format!("9 sq ft {rest}")))
            .collect();
        let past = [
            changed(&shapes[0], "40 sq ft Quartz Taj", "Quartz Taj"),
            changed(&shapes[1], "12.5 sq ft Granite Luna", "Granite Luna"),
            changed(&shapes[2], "9 sq ft Quartz Frost", "Quartz Frost"),
            changed(&shapes[3], "7.3 sq ft - 4\" Splash", "4\" Splash"),
        ];
        let leads = Leads::learn(&past);
        assert_eq!(
            leads.strip("55.1 sq ft Granite Black Pearl").as_deref(),
            Some("Granite Black Pearl")
        );
        assert_eq!(
            leads.strip("3 sq ft - 6\" Splash").as_deref(),
            Some("6\" Splash")
        );
        // Another start, and a line without a count, stay.
        assert_eq!(leads.strip("14 lin ft Finished Edge - Flat"), None);
        assert_eq!(leads.strip("Laser Template"), None);
    }

    #[test]
    fn a_line_that_last_kept_its_start_keeps_it() {
        let sealer = shape("9 sq ft - 10 year Sealer");
        let shapes: Vec<String> = ["Quartz Taj", "Granite Luna", "Quartz Frost", "Marble Zeus"]
            .iter()
            .map(|rest| shape(&format!("9 sq ft {rest}")))
            .collect();
        let mut past = vec![kept(&sealer, "27.7 sq ft - 10 year Sealer")];
        past.extend([
            changed(&shapes[0], "40 sq ft Quartz Taj", "Quartz Taj"),
            changed(&shapes[1], "12.5 sq ft Granite Luna", "Granite Luna"),
            changed(&shapes[2], "9 sq ft Quartz Frost", "Quartz Frost"),
            changed(&shapes[3], "9 sq ft Marble Zeus", "Marble Zeus"),
            // Older: the sealer lost it once.
            changed(&sealer, "30 sq ft - 10 year Sealer", "10 year Sealer"),
        ]);
        let leads = Leads::learn(&past);
        assert_eq!(leads.strip("11 sq ft - 10 year Sealer"), None);
        assert_eq!(
            leads.strip("11 sq ft Quartz Calacatta").as_deref(),
            Some("Quartz Calacatta")
        );
    }

    #[test]
    fn two_new_lines_keeping_their_start_turn_the_rule_off() {
        let shapes: Vec<String> = ["A", "B", "C", "D", "E", "F"]
            .iter()
            .map(|rest| shape(&format!("9 sq ft Quartz {rest}")))
            .collect();
        let lost = |n: usize| LeadPast {
            shape: &shapes[n],
            outcome: "changed",
            value: Some("Quartz"),
            draft_text: "9 sq ft Quartz",
        };
        let mut past = vec![lost(0), lost(1), lost(2), lost(3)];
        assert!(Leads::learn(&past).ruled.contains("# sq ft"));
        past.insert(0, kept(&shapes[4], "9 sq ft Quartz E"));
        assert!(Leads::learn(&past).ruled.contains("# sq ft"));
        past.insert(0, kept(&shapes[5], "9 sq ft Quartz F"));
        assert!(Leads::learn(&past).ruled.is_empty());
    }

    #[test]
    fn lines_typed_in_are_told_from_draft_lines_with_where_they_stand() {
        let draft = [
            text("a", "Kitchen based @ 26.6 sqft"),
            text("b", "Laser Template"),
        ];
        let saved = [
            text("x", "Thank you for your business"),
            text("a", "Kitchen 26.6"),
            text("new:1", "Sinks provided by customer"),
            text("new:usual-0", "Tax is included"),
            text("b", "Laser Template"),
        ];
        let got: Vec<(String, String)> = added_lines(&draft, &saved)
            .into_iter()
            .map(|line| (line.text, line.after))
            .collect();
        assert_eq!(
            got,
            [
                ("Thank you for your business".to_string(), String::new()),
                (
                    "Sinks provided by customer".to_string(),
                    shape("Kitchen based @ 26.6 sqft")
                ),
                (
                    "Tax is included".to_string(),
                    format!("+{}", shape("Sinks provided by customer"))
                ),
            ]
        );
    }

    fn row(id: i32, invoice: &str, typed: Option<(&str, &str)>) -> AddedRow {
        AddedRow {
            id,
            qbo_invoice_id: invoice.into(),
            outcome: if typed.is_some() { "added" } else { "none" }.into(),
            draft_shape: typed.map(|(text, _)| shape(text)),
            draft_text: typed.map(|(text, _)| text.to_string()),
            saved_value: typed.map(|(_, after)| after.to_string()),
            by_hand: None,
        }
    }

    #[test]
    fn a_text_typed_into_the_latest_invoices_is_added_where_it_goes() {
        let sink = "Sinks provided by customer";
        let after = shape("1 Sink - 50/50");
        let rows = [
            row(9, "5", Some((sink, &after))),
            row(8, "5", Some(("Measured 34.39 sqft", ""))),
            row(7, "4", Some((sink, &after))),
            row(6, "3", None),
            row(5, "2", Some((sink, &after))),
            row(4, "2", Some(("Measured 80 sqft", ""))),
        ];
        let (history, usual) = added_history(&rows, &HashMap::new(), &HashMap::new());
        // The measurement differs each time: nothing to add.
        assert_eq!(history.len(), 1);
        let outcomes: Vec<&str> = history[&shape(sink)]
            .iter()
            .map(|past| past.outcome.outcome.as_str())
            .collect();
        assert_eq!(outcomes, ["added", "added", "absent", "added"]);
        let mut lines = vec![
            text("a", "Kitchen based @ 26.6 sqft"),
            text("b", "Sink"),
            text("c", "Laser Template"),
        ];
        let drafted = HashMap::from([
            ("a".to_string(), shape("Kitchen based @ 26.6 sqft")),
            ("b".to_string(), shape("1 Sink - 50/50")),
            ("c".to_string(), shape("Laser Template")),
        ]);
        let mut wanted: Vec<Usual> = usual.into_values().collect();
        wanted.push(Usual {
            text: "Tax is included".into(),
            after: format!("+{}", shape(sink)),
            order: 10,
        });
        wanted.push(Usual {
            text: "Laser template".into(),
            after: shape("Not on this quote"),
            order: 11,
        });
        wanted.push(Usual {
            text: "Thank you".into(),
            after: shape("Not on this quote"),
            order: 12,
        });
        insert(&mut lines, &drafted, wanted);
        let texts: Vec<&str> = lines.iter().map(said).collect();
        assert_eq!(
            texts,
            [
                "Kitchen based @ 26.6 sqft",
                "Sink",
                sink,
                "Tax is included",
                "Laser Template",
                "Thank you"
            ]
        );
        assert!(is_usual(&lines[2]) && is_text(&lines[2]) && lines[2].amount.is_none());
    }
}
