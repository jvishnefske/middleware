use serde::{Deserialize, Serialize};

use crate::page::Page;

/// The outcome of evaluating one page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// True when no hard rule failed and `score` reached the threshold.
    pub accepted: bool,
    /// Share of the wanted items found, from 0.0 to 1.0.
    pub score: f64,
    /// Wanted items that were found.
    pub matched: Vec<String>,
    /// Wanted items that were not found.
    pub missing: Vec<String>,
    /// Hard rules that failed, in plain words. Any entry means rejection.
    pub rejected_by: Vec<String>,
}

/// Stage 2: judges a fetched page.
///
/// [`Criteria`] is the data-driven implementation; [`Composite`] combines
/// several evaluators. Write your own for anything that needs more than text
/// matching.
pub trait Evaluator: Sync {
    fn evaluate(&self, page: &Page) -> Verdict;
}

/// Keyword and link rules for a page. Matching is case-insensitive
/// substring matching over the page's URL, title and visible text, and over
/// link URLs and link texts for `any_links`, where the page's own URL also
/// counts as a link. A PDF shows no text in the browser, but its URL and file
/// name often carry the part number.
///
/// Hard rules (any failure rejects the page): every `require` term present;
/// at least `any_min` of `any_of` present (default 1 when `any_of` is set);
/// no `forbid` term present; at least one of `any_links` among the links.
///
/// Score: the share of wanted items found, where wanted items are each
/// `require` term, each `any_of` term and the `any_links` group. The page is
/// accepted when no hard rule failed and the score is at least `min_score`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Criteria {
    pub require: Vec<String>,
    pub any_of: Vec<String>,
    pub any_min: usize,
    pub forbid: Vec<String>,
    pub any_links: Vec<String>,
    pub min_score: f64,
}

impl Evaluator for Criteria {
    fn evaluate(&self, page: &Page) -> Verdict {
        let text = format!("{} {} {}", page.url, page.title, page.text()).to_lowercase();
        // The page itself is a link target too: a direct PDF has no links on it.
        let mut links: Vec<String> = page
            .links()
            .iter()
            .map(|l| format!("{} {}", l.url, l.text).to_lowercase())
            .collect();
        links.push(format!("{} {}", page.url, page.title).to_lowercase());
        let has = |term: &str| text.contains(&term.to_lowercase());
        let link_has = |term: &str| {
            let term = term.to_lowercase();
            links.iter().any(|l| l.contains(&term))
        };

        let mut matched = Vec::new();
        let mut missing = Vec::new();
        let mut rejected_by = Vec::new();
        let mut hits = 0usize;
        let mut wanted = 0usize;

        for term in &self.require {
            wanted += 1;
            if has(term) {
                hits += 1;
                matched.push(term.clone());
            } else {
                missing.push(term.clone());
                rejected_by.push(format!("required term missing: {term}"));
            }
        }

        let mut alternatives_found = 0usize;
        for term in &self.any_of {
            wanted += 1;
            if has(term) {
                alternatives_found += 1;
                hits += 1;
                matched.push(term.clone());
            } else {
                missing.push(term.clone());
            }
        }
        if !self.any_of.is_empty() {
            let need = self.any_min.max(1);
            if alternatives_found < need {
                rejected_by.push(format!(
                    "{alternatives_found} of the alternatives found, {need} needed"
                ));
            }
        }

        for term in &self.forbid {
            if has(term) {
                rejected_by.push(format!("forbidden term present: {term}"));
            }
        }

        if !self.any_links.is_empty() {
            wanted += 1;
            let found: Vec<&String> = self.any_links.iter().filter(|t| link_has(t)).collect();
            if found.is_empty() {
                missing.push(format!("link matching one of {:?}", self.any_links));
                rejected_by.push(format!("no link matches any of {:?}", self.any_links));
            } else {
                hits += 1;
                matched.push(format!("link: {}", found[0]));
            }
        }

        let score = if wanted == 0 {
            1.0
        } else {
            hits as f64 / wanted as f64
        };
        Verdict {
            accepted: rejected_by.is_empty() && score >= self.min_score,
            score,
            matched,
            missing,
            rejected_by,
        }
    }
}

/// How a [`Composite`] combines its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    /// Accepted only if every part accepts. Score is the mean.
    All,
    /// Accepted if any part accepts. Score is the best part's.
    Any,
}

/// Several evaluators combined under one verdict.
pub struct Composite {
    pub combine: Combine,
    pub parts: Vec<Box<dyn Evaluator>>,
}

impl Evaluator for Composite {
    fn evaluate(&self, page: &Page) -> Verdict {
        let verdicts: Vec<Verdict> = self.parts.iter().map(|p| p.evaluate(page)).collect();
        let mut out = Verdict {
            accepted: false,
            score: 0.0,
            matched: Vec::new(),
            missing: Vec::new(),
            rejected_by: Vec::new(),
        };
        if verdicts.is_empty() {
            out.accepted = true;
            out.score = 1.0;
            return out;
        }
        for v in &verdicts {
            out.matched.extend(v.matched.iter().cloned());
            out.missing.extend(v.missing.iter().cloned());
            out.rejected_by.extend(v.rejected_by.iter().cloned());
        }
        match self.combine {
            Combine::All => {
                out.accepted = verdicts.iter().all(|v| v.accepted);
                out.score = verdicts.iter().map(|v| v.score).sum::<f64>() / verdicts.len() as f64;
            }
            Combine::Any => {
                out.accepted = verdicts.iter().any(|v| v.accepted);
                out.score = verdicts.iter().map(|v| v.score).fold(0.0, f64::max);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATASHEET: &str = r#"<html><head><title>nRF52840 Product Specification</title></head>
        <body><p>The nRF52840 SoC. Revision 1.2, firmware v3.4.</p>
        <a href="/files/nrf52840_ps.pdf">Product spec (PDF)</a></body></html>"#;

    fn page(html: &str) -> Page {
        Page::new("https://vendor.example/nrf52840", "", html)
    }

    fn criteria(json: &str) -> Criteria {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn accepts_when_all_hard_rules_hold() {
        let c = criteria(
            r#"{"require":["nrf52840"],"any_of":["specification"],
                "forbid":["buy now"],"any_links":[".pdf"],"min_score":1.0}"#,
        );
        let v = c.evaluate(&page(DATASHEET));
        assert!(v.accepted, "{v:?}");
        assert_eq!(v.score, 1.0);
        assert!(v.rejected_by.is_empty());
    }

    #[test]
    fn rejects_on_missing_required_term_with_reason() {
        let c = criteria(r#"{"require":["nrf52832"]}"#);
        let v = c.evaluate(&page(DATASHEET));
        assert!(!v.accepted);
        assert_eq!(
            v.rejected_by,
            vec!["required term missing: nrf52832".to_string()]
        );
    }

    #[test]
    fn rejects_on_forbidden_term_even_with_full_score() {
        let c = criteria(r#"{"require":["nrf52840"],"forbid":["product specification"]}"#);
        let v = c.evaluate(&page(DATASHEET));
        assert!(!v.accepted);
        assert_eq!(v.score, 1.0);
    }

    #[test]
    fn min_score_turns_partial_matches_into_rejection() {
        let c = criteria(
            r#"{"require":["nrf52840"],"any_of":["specification","errata","schematic"],
                "any_min":1,"min_score":0.9}"#,
        );
        let v = c.evaluate(&page(DATASHEET));
        // Wanted: 1 required + 3 alternatives = 4; found: nrf52840, specification.
        assert!(!v.accepted);
        assert!((v.score - 0.5).abs() < 1e-9, "score {}", v.score);
        assert!(v.missing.contains(&"errata".to_string()));
        assert!(
            v.rejected_by.is_empty(),
            "hard rules hold; only the score fails"
        );
    }

    #[test]
    fn any_links_needs_a_matching_link() {
        let c = criteria(r#"{"any_links":[".zip",".exe"]}"#);
        let v = c.evaluate(&page(DATASHEET));
        assert!(!v.accepted);
        assert!(v.rejected_by[0].contains("no link matches"));
    }

    #[test]
    fn empty_criteria_accept_everything() {
        let v = Criteria::default().evaluate(&page(DATASHEET));
        assert!(v.accepted);
        assert_eq!(v.score, 1.0);
    }

    #[test]
    fn url_counts_as_evidence_for_documents_without_text() {
        // A PDF in the browser's viewer: no text in the DOM, the file name is the title.
        let pdf = Page::new(
            "https://vendor.example/nRF52840_PS_v1.5.pdf",
            "nRF52840_PS_v1.5.pdf",
            "<embed type=\"application/pdf\">",
        );
        let v = criteria(r#"{"require":["nrf52840"],"any_links":[]}"#).evaluate(&pdf);
        assert!(v.accepted, "{v:?}");
    }

    #[test]
    fn scripts_do_not_count_as_content() {
        let html = "<html><title>t</title><script>secret-token</script></html>";
        let v = criteria(r#"{"require":["secret-token"]}"#).evaluate(&page(html));
        assert!(!v.accepted);
    }

    #[test]
    fn composite_all_and_any() {
        let pass = || Box::new(criteria(r#"{"require":["nrf52840"]}"#)) as Box<dyn Evaluator>;
        let fail = || Box::new(criteria(r#"{"require":["absent-term"]}"#)) as Box<dyn Evaluator>;

        let all = Composite {
            combine: Combine::All,
            parts: vec![pass(), fail()],
        };
        assert!(!all.evaluate(&page(DATASHEET)).accepted);

        let any = Composite {
            combine: Combine::Any,
            parts: vec![pass(), fail()],
        };
        let v = any.evaluate(&page(DATASHEET));
        assert!(v.accepted);
        assert!(v.rejected_by.iter().any(|r| r.contains("absent-term")));
    }
}
