//! The product a new invoice's priced lines get, learned per company from its
//! own invoices, with nothing about any product written in code.
//!
//! Two kinds of examples, each a product with the words of its lines:
//! invoices reps created here (the quote's words and the product they saved,
//! [`PAIR_WEIGHT`] times as telling, since a rep chose it for that quote), and
//! the company's newest `QuickBooks` invoices (`qbo_invoice_scans`), the newer
//! the more telling ([`HALF_LIFE`]). A draft gets the product whose examples
//! read most like it: which words they have and which they lack (naive Bayes
//! over those words), weighed by how much the product is used. So a company
//! keeps getting what it keeps doing, and a few corrections turn it.
//!
//! An office may word its invoices nothing like the drafts made from quotes
//! ("Price based on color" against "Kitchen based @ 71.3 sqft ..."). Three
//! things keep that from picking a wrong product:
//! - A product reps chose for quotes at least [`TRUSTED_CHOICES`] times is
//!   read from those quotes' words alone, which are worded like the draft.
//! - A product with few examples says little about its words: its shares are
//!   pulled toward what all the company's examples say ([`PRIOR_EXAMPLES`]).
//! - A product reps never chose that is hardly used ([`MIN_USE`] of the most
//!   used one) is not a candidate: two odd invoices do not outvote the
//!   product on nearly every invoice.

use super::rules::Seen;
use std::collections::{HashMap, HashSet};

/// Words on fewer examples than this tell nothing about a product.
const MIN_WORD_INVOICES: usize = 3;
/// How much more an invoice a rep created from a quote counts than a scanned one.
const PAIR_WEIGHT: f64 = 3.0;
/// Scanned invoices (newest first) count half as much every this many.
const HALF_LIFE: f64 = 100.0;
/// A product's word shares start from the company's overall shares as if it
/// had this many examples saying so.
const PRIOR_EXAMPLES: f64 = 5.0;
/// Reps' choices of a product (with the quote's words) that are enough to
/// read the product from them alone.
const TRUSTED_CHOICES: usize = 2;
/// The least a product reps never chose must be used, as a share of the
/// most used product, to be a candidate.
const MIN_USE: f64 = 0.2;

/// Weighted examples, and the weight on which each word is.
#[derive(Default)]
struct Read<'a> {
    examples: f64,
    words: HashMap<&'a str, f64>,
}

impl<'a> Read<'a> {
    fn add(&mut self, words: &'a [String], weight: f64) {
        self.examples += weight;
        for word in words {
            *self.words.entry(word.as_str()).or_default() += weight;
        }
    }

    /// The share of the examples with the word, starting from `overall`
    /// (the share of all the company's examples with it). Never 0 or 1, so
    /// one word cannot rule a product in or out.
    fn share(&self, word: &str, overall: f64) -> f64 {
        let with = self.words.get(word).copied().unwrap_or(0.0);
        (PRIOR_EXAMPLES.mul_add(overall, with) / (self.examples + PRIOR_EXAMPLES))
            .clamp(0.005, 0.995)
    }
}

/// One product as the company's invoices use it.
struct Product<'a> {
    value: &'a str,
    name: Option<&'a str>,
    /// Its examples, weighted.
    used: f64,
    /// Those whose words are known.
    all: Read<'a>,
    /// Those of them reps chose for a quote, and how many.
    chosen: Read<'a>,
    choices: usize,
}

impl Product<'_> {
    /// Reps chose it for quotes often enough to read it from those alone.
    const fn trusted(&self) -> bool {
        self.choices >= TRUSTED_CHOICES
    }
}

/// A word that tells products apart, with the share of the examples it is
/// on: of all of them, and of those reps chose for a quote.
struct Word<'a> {
    text: &'a str,
    overall: f64,
    chosen: f64,
}

/// What a company's invoices say about its products.
pub(super) struct Model<'a> {
    products: Vec<Product<'a>>,
    /// The words that tell products apart (on [`MIN_WORD_INVOICES`] or more).
    vocabulary: Vec<Word<'a>>,
}

impl<'a> Model<'a> {
    /// Learned from a product history: invoices created here, then scanned
    /// ones, each newest first.
    pub(super) fn learn(history: &'a [Seen]) -> Self {
        let mut products: Vec<Product> = Vec::new();
        let mut examples_with: HashMap<&str, usize> = HashMap::new();
        let mut scanned = 0.0;
        for seen in history.iter().filter(|seen| seen.outcome == "value") {
            let Some(value) = seen.value.as_deref() else {
                continue;
            };
            let weight = if seen.pair {
                PAIR_WEIGHT
            } else {
                scanned += 1.0;
                0.5_f64.powf((scanned - 1.0) / HALF_LIFE)
            };
            let index = match products.iter().position(|p| p.value == value) {
                Some(index) => index,
                None => {
                    products.push(Product {
                        value,
                        name: None,
                        used: 0.0,
                        all: Read::default(),
                        chosen: Read::default(),
                        choices: 0,
                    });
                    products.len() - 1
                }
            };
            let product = &mut products[index];
            product.used += weight;
            product.name = product.name.or(seen.name.as_deref());
            if !seen.words.is_empty() {
                product.all.add(&seen.words, weight);
                if seen.pair {
                    product.chosen.add(&seen.words, weight);
                    product.choices += 1;
                }
                for word in &seen.words {
                    *examples_with.entry(word.as_str()).or_default() += 1;
                }
            }
        }
        let (all, chosen) = products.iter().fold((0.0, 0.0), |(all, chosen), product| {
            (all + product.all.examples, chosen + product.chosen.examples)
        });
        let share = |with: f64, of: f64| if of > 0.0 { with / of } else { 0.5 };
        let mut vocabulary: Vec<Word> = examples_with
            .into_iter()
            .filter(|(_, count)| *count >= MIN_WORD_INVOICES)
            .map(|(text, _)| {
                let with_all: f64 = products
                    .iter()
                    .filter_map(|product| product.all.words.get(text))
                    .sum();
                let with_chosen: f64 = products
                    .iter()
                    .filter_map(|product| product.chosen.words.get(text))
                    .sum();
                Word {
                    text,
                    overall: share(with_all, all),
                    chosen: share(with_chosen, chosen),
                }
            })
            .collect();
        vocabulary.sort_unstable_by(|a, b| a.text.cmp(b.text));
        Self {
            products,
            vocabulary,
        }
    }

    fn score(&self, product: &Product, draft: &HashSet<&str>) -> f64 {
        let total = self.products.iter().map(|p| p.used + 1.0).sum::<f64>();
        let prior = (product.used + 1.0).ln() - total.ln();
        prior
            + self
                .vocabulary
                .iter()
                .map(|word| {
                    // Quotes are compared with quotes, invoices with all.
                    let share = if product.trusted() {
                        product.chosen.share(word.text, word.chosen)
                    } else {
                        product.all.share(word.text, word.overall)
                    };
                    if draft.contains(word.text) {
                        share.ln()
                    } else {
                        (1.0 - share).ln()
                    }
                })
                .sum::<f64>()
    }

    /// The product for a draft with these words, but none of `avoid`; the
    /// newest wins a tie.
    pub(super) fn pick(
        &self,
        draft: &[String],
        avoid: &HashSet<String>,
    ) -> Option<(String, Option<String>)> {
        let draft: HashSet<&str> = draft.iter().map(String::as_str).collect();
        let allowed = || {
            self.products
                .iter()
                .filter(|product| !avoid.contains(product.value))
        };
        let most = allowed().map(|product| product.used).fold(0.0, f64::max);
        let winner = allowed()
            .filter(|product| product.choices > 0 || product.used >= most * MIN_USE)
            .map(|product| (product, self.score(product, &draft)))
            .fold(
                None,
                |best: Option<(&Product, f64)>, (product, s)| match best {
                    Some(best) if best.1 >= s => Some(best),
                    _ => Some((product, s)),
                },
            )?
            .0;
        Some((winner.value.to_string(), winner.name.map(str::to_string)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::rules::words;
    use super::*;

    fn item(value: &str, name: &str, lines: &[&str]) -> Seen {
        Seen {
            outcome: "value".into(),
            value: Some(value.into()),
            name: Some(name.into()),
            words: words(lines.iter().copied()),
            pair: false,
        }
    }

    const PACKAGE: [&str; 4] = [
        "Kitchen based @ 70 sqft",
        "Granite Black Pearl",
        "12 Shaker Cabinets - White",
        "Installation",
    ];
    const COUNTERS: [&str; 3] = ["Granite Black Pearl", "Fabrication", "Installation"];

    /// The package is the most used, but only ever with cabinets.
    fn history() -> Vec<Seen> {
        let mut history: Vec<Seen> = (0..6)
            .map(|_| item("30", "Package $11999", &PACKAGE))
            .collect();
        history.extend((0..4).map(|_| item("5", "Sales", &COUNTERS)));
        history.push(item("9", "Square Footage", &COUNTERS));
        history
    }

    fn draft(lines: &[&str]) -> Vec<String> {
        words(lines.iter().copied())
    }

    #[test]
    fn the_product_follows_what_the_companys_invoices_with_such_lines_use() {
        let history = history();
        let none = HashSet::new();
        let model = Model::learn(&history);
        assert_eq!(
            model.pick(&draft(&COUNTERS), &none),
            Some(("5".into(), Some("Sales".into())))
        );
        assert_eq!(
            model.pick(&draft(&PACKAGE), &none),
            Some(("30".into(), Some("Package $11999".into())))
        );
        // Avoided, it is never picked.
        let avoid = HashSet::from(["30".to_string()]);
        assert_ne!(model.pick(&draft(&PACKAGE), &avoid).unwrap().0, "30");
        // Choices without words: the most used.
        let saved: Vec<Seen> = history
            .iter()
            .map(|seen| Seen {
                words: Vec::new(),
                ..seen.clone()
            })
            .collect();
        assert_eq!(
            Model::learn(&saved)
                .pick(&draft(&["Fabrication"]), &none)
                .unwrap()
                .0,
            "30"
        );
        assert_eq!(
            Model::learn(&[]).pick(&draft(&["Fabrication"]), &none),
            None
        );
    }

    #[test]
    fn a_company_keeps_its_habit_until_reps_correct_it() {
        // Every recent QuickBooks invoice got the package, cabinets or not.
        let mut scanned: Vec<Seen> = (0..60)
            .map(|_| item("30", "Package $11999", &COUNTERS))
            .collect();
        scanned.extend((0..10).map(|_| item("30", "Package $11999", &PACKAGE)));
        scanned.extend((0..30).map(|_| item("5", "Sales", &COUNTERS)));
        let none = HashSet::new();
        assert_eq!(
            Model::learn(&scanned)
                .pick(&draft(&COUNTERS), &none)
                .unwrap()
                .0,
            "30"
        );
        // Reps pick Sales for countertop-only quotes: a few such choices turn it.
        let mut history: Vec<Seen> = (0..8)
            .map(|_| Seen {
                pair: true,
                ..item("5", "Sales", &COUNTERS)
            })
            .collect();
        history.extend(scanned.iter().cloned());
        let model = Model::learn(&history);
        assert_eq!(model.pick(&draft(&COUNTERS), &none).unwrap().0, "5");
        assert_eq!(model.pick(&draft(&PACKAGE), &none).unwrap().0, "30");
    }

    /// An office that words its invoices its own way: nothing like the
    /// drafts made from quotes.
    #[test]
    fn a_product_used_twice_does_not_beat_the_one_on_nearly_every_invoice() {
        let mut scanned: Vec<Seen> = (0..179)
            .map(|n| {
                let lines: &[&str] = if n % 2 == 0 {
                    &[
                        "Price based on color",
                        "Undermount sink cut out",
                        "Customers provided sinks",
                    ]
                } else {
                    &[
                        "Price based on color",
                        "Tear out formica laminate wood",
                        "Fee per mile",
                    ]
                };
                item("7", "Kitchen, Master & Guest Bath, Laundry", lines)
            })
            .collect();
        scanned.extend((0..11).map(|_| item("9", "Cost of Slab", &["Cost of slab, tax included"])));
        scanned.extend((0..2).map(|_| item("3", "Labor", &["Fabrication", "Installation"])));
        let quote = draft(&[
            "Kitchen based @ 71.3 sqft Additional sqft of Taj Beach @ $70/sqft",
            "71.3 sq ft Quartz Taj Beach",
            "71.3 sq ft Fabrication",
            "71.3 sq ft Installation",
            "86.3 lin ft Finished Edge - Eased",
            "1 - 30\" Undermount Sink Cutout",
            "Laminate T/O (Over 60 sqft)",
        ]);
        let none = HashSet::new();
        // Labor's two invoices say "fabrication" and "installation" like the
        // draft, and little is known against it; the product on 179 of 192
        // invoices still wins.
        assert_eq!(Model::learn(&scanned).pick(&quote, &none).unwrap().0, "7");
        let chose = |value: &str, name: &str| Seen {
            pair: true,
            words: quote.clone(),
            ..item(value, name, &[])
        };
        // Reps created six invoices with Labor and three with the usual
        // product, all for such quotes: the quotes' words tell the two apart
        // no more, and the product the company uses most stays.
        let mut mixed: Vec<Seen> = (0..3)
            .map(|_| chose("7", "Kitchen, Master & Guest Bath, Laundry"))
            .collect();
        mixed.extend((0..6).map(|_| chose("3", "Labor")));
        mixed.extend(scanned.iter().cloned());
        assert_eq!(Model::learn(&mixed).pick(&quote, &none).unwrap().0, "7");
        // Reps picking only Labor for such quotes, again and again, turn it.
        let mut history: Vec<Seen> = (0..12).map(|_| chose("3", "Labor")).collect();
        history.extend(scanned);
        assert_eq!(Model::learn(&history).pick(&quote, &none).unwrap().0, "3");
    }

    #[test]
    fn newer_invoices_count_more() {
        // The company moved from Sales to the package: the newest habit wins.
        let mut history: Vec<Seen> = (0..50)
            .map(|_| item("30", "Package $11999", &COUNTERS))
            .collect();
        history.extend((0..70).map(|_| item("5", "Sales", &COUNTERS)));
        assert_eq!(
            Model::learn(&history)
                .pick(&draft(&COUNTERS), &HashSet::new())
                .unwrap()
                .0,
            "30"
        );
    }
}
