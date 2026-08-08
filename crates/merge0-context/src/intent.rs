//! Intent docs: MERGE0.md parsing and the fence rule.
//!
//! The fence rule (PRD §5c): any Merge0 process may write **only** inside
//! the explicitly marked machine-managed section; customer-authored prose is
//! never rewritten. This module is that enforcement — the hardening pass has
//! no other write path into intent docs.

pub const FENCE_START: &str = "<!-- merge0:managed:start -->";
pub const FENCE_END: &str = "<!-- merge0:managed:end -->";

/// The shipped MERGE0.md template (PRD §6a onboarding): fenced machine
/// section pre-marked.
pub const MERGE0_TEMPLATE: &str = "\
# MERGE0.md — intent notes for automated triage

Tell Merge0 what your product is *supposed* to do. Triage reads this before
judging whether a signal is a defect. Short, factual notes beat prose.

## Invariants

- (example) A school may exist without a linked district while onboarding.

## This is a feature, not a bug

- (example) Export intentionally omits archived rosters.

## Constraints for generated fixes

- (example) Never change sync scheduling logic without human design review.

<!-- merge0:managed:start -->
<!-- merge0:managed:end -->
";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FenceError {
    #[error("intent doc has no machine-managed fence")]
    MissingFence,
    #[error("intent doc has malformed fences (start/end mismatch or repeated)")]
    MalformedFence,
}

/// A parsed intent doc: human prose plus the machine-managed section.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentDoc {
    text: String,
    fence_inner_start: usize,
    fence_inner_end: usize,
}

impl IntentDoc {
    pub fn parse(text: &str) -> Result<IntentDoc, FenceError> {
        let starts: Vec<_> = text.match_indices(FENCE_START).collect();
        let ends: Vec<_> = text.match_indices(FENCE_END).collect();
        match (starts.as_slice(), ends.as_slice()) {
            ([], []) => Err(FenceError::MissingFence),
            ([(start, _)], [(end, _)]) if start < end => Ok(IntentDoc {
                text: text.to_string(),
                fence_inner_start: start + FENCE_START.len(),
                fence_inner_end: *end,
            }),
            _ => Err(FenceError::MalformedFence),
        }
    }

    pub fn full_text(&self) -> &str {
        &self.text
    }

    /// Everything the customer wrote — what the gate reads as constraints.
    pub fn human_text(&self) -> String {
        let mut human = String::new();
        human.push_str(&self.text[..self.fence_inner_start - FENCE_START.len()]);
        human.push_str(&self.text[self.fence_inner_end + FENCE_END.len()..]);
        human
    }

    pub fn machine_section(&self) -> &str {
        self.text[self.fence_inner_start..self.fence_inner_end].trim_matches('\n')
    }

    /// Produce a new doc with the machine section replaced — the ONLY write
    /// path into an intent doc. Human prose is preserved byte-for-byte.
    pub fn with_machine_section(&self, content: &str) -> IntentDoc {
        let mut text = String::with_capacity(self.text.len() + content.len());
        text.push_str(&self.text[..self.fence_inner_start]);
        text.push('\n');
        let trimmed = content.trim_matches('\n');
        if !trimmed.is_empty() {
            text.push_str(trimmed);
            text.push('\n');
        }
        text.push_str(&self.text[self.fence_inner_end..]);
        IntentDoc::parse(&text).expect("rewriting the fence preserves fence structure")
    }

    /// Append one line to the machine section (dedup-aware).
    pub fn append_machine_line(&self, line: &str) -> IntentDoc {
        let current = self.machine_section();
        if current.lines().any(|l| l.trim() == line.trim()) {
            return self.clone();
        }
        let updated = if current.is_empty() {
            line.trim().to_string()
        } else {
            format!("{current}\n{}", line.trim())
        };
        self.with_machine_section(&updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_with_empty_machine_section() {
        let doc = IntentDoc::parse(MERGE0_TEMPLATE).unwrap();
        assert_eq!(doc.machine_section(), "");
        assert!(doc.human_text().contains("Invariants"));
    }

    #[test]
    fn missing_or_malformed_fences_are_errors() {
        assert_eq!(
            IntentDoc::parse("# doc without fence"),
            Err(FenceError::MissingFence)
        );
        let double = format!("{FENCE_START}\n{FENCE_END}\n{FENCE_START}\n{FENCE_END}");
        assert_eq!(IntentDoc::parse(&double), Err(FenceError::MalformedFence));
        let inverted = format!("{FENCE_END}\n{FENCE_START}");
        assert_eq!(IntentDoc::parse(&inverted), Err(FenceError::MalformedFence));
    }

    #[test]
    fn machine_write_preserves_human_prose_byte_for_byte() {
        let original = format!(
            "# My rules\n\n- precious human prose — don't touch\n\n{FENCE_START}\nold line\n{FENCE_END}\n\n## More human text\n"
        );
        let doc = IntentDoc::parse(&original).unwrap();
        let human_before = doc.human_text();

        let updated = doc.with_machine_section("- new rule (from report 01ABC)");
        assert_eq!(updated.machine_section(), "- new rule (from report 01ABC)");
        assert_eq!(
            updated.human_text(),
            human_before,
            "human prose must be untouched"
        );
        assert!(updated.full_text().contains("precious human prose"));
        assert!(!updated.full_text().contains("old line"));
    }

    #[test]
    fn append_deduplicates() {
        let doc = IntentDoc::parse(MERGE0_TEMPLATE).unwrap();
        let once = doc.append_machine_line("- rule A");
        let twice = once.append_machine_line("- rule A");
        assert_eq!(once, twice);
        let more = twice.append_machine_line("- rule B");
        assert_eq!(more.machine_section(), "- rule A\n- rule B");
    }
}

/// One `##` section of an intent doc, plus the machine-managed fence as a
/// section of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentSection {
    /// Heading text without the `#` markers; synthesized for the fence and
    /// for any preamble before the first heading.
    pub heading: String,
    pub body: String,
    /// True for the fenced machine section — amendments Merge0 earned from
    /// real incidents, not customer prose.
    pub managed: bool,
}

impl IntentSection {
    fn len(&self) -> usize {
        self.heading.len() + self.body.len() + 2
    }

    fn render(&self) -> String {
        if self.heading.is_empty() {
            self.body.clone()
        } else {
            format!("## {}\n{}", self.heading, self.body)
        }
    }
}

const MANAGED_HEADING: &str = "Merge0-managed constraints (earned from past incidents)";

/// Split an intent doc into sections, **including** the machine-managed
/// fence. Docs without a fence (or with a malformed one) are split on
/// headings alone — a customer's doc is never rejected for shape.
pub fn sections(text: &str) -> Vec<IntentSection> {
    let (human, managed) = match IntentDoc::parse(text) {
        Ok(doc) => (doc.human_text(), doc.machine_section().to_string()),
        Err(_) => (text.to_string(), String::new()),
    };

    let mut out = Vec::new();
    let mut heading = String::new();
    let mut body = String::new();
    for line in human.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if !heading.is_empty() || !body.trim().is_empty() {
                out.push(IntentSection {
                    heading: std::mem::take(&mut heading),
                    body: std::mem::take(&mut body).trim().to_string(),
                    managed: false,
                });
            }
            heading = rest.trim().to_string();
            body.clear();
        } else if line.starts_with("# ") {
            // Document title: not a section of its own.
            continue;
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if !heading.is_empty() || !body.trim().is_empty() {
        out.push(IntentSection {
            heading,
            body: body.trim().to_string(),
            managed: false,
        });
    }
    out.retain(|s| !s.body.trim().is_empty() || !s.heading.is_empty());

    if !managed.trim().is_empty() {
        out.push(IntentSection {
            heading: MANAGED_HEADING.to_string(),
            body: managed.trim().to_string(),
            managed: true,
        });
    }
    out
}

/// Select the intent a gate decision actually needs, within `budget`
/// characters — and say so when anything is left out.
///
/// Replaces blind head-truncation, which silently dropped whatever sat at
/// the end of the doc. That was not a cosmetic problem: the shipped
/// `MERGE0.md` template puts *Constraints for generated fixes* last and the
/// machine fence last of all, so the first things cut were the constraints
/// and the amendments Merge0 itself earned from past incidents — exactly
/// the content that prevents a bad Work Order.
///
/// Order: machine-managed amendments first (they are preventions with an
/// incident behind them), then sections that share vocabulary with the
/// report, with a boost for headings that read like constraints. Sections
/// are kept whole so a rule is never half-shown. Anything omitted is named
/// in a trailing note, so the gate knows it is deciding on partial intent
/// and can be conservative rather than confidently wrong.
pub fn relevant_intent(text: &str, focus: &str, budget: usize) -> String {
    let all = sections(text);
    if all.is_empty() {
        return String::new();
    }

    let terms: Vec<String> = focus
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 3)
        .map(|t| t.to_ascii_lowercase())
        .collect();

    let mut ranked: Vec<(usize, i64, &IntentSection)> = all
        .iter()
        .enumerate()
        .map(|(order, section)| (order, score(section, &terms), section))
        .collect();
    // Managed first, then score desc, then document order — fully
    // deterministic, so the same report always sees the same intent.
    ranked.sort_by(|a, b| {
        b.2.managed
            .cmp(&a.2.managed)
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    let mut kept: Vec<(usize, String)> = Vec::new();
    let mut omitted: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for (order, _, section) in &ranked {
        if used + section.len() <= budget {
            used += section.len();
            kept.push((*order, section.render()));
        } else if kept.is_empty() {
            // A single oversized section: show what fits rather than
            // nothing, and still disclose below.
            kept.push((*order, truncate_to(&section.render(), budget)));
            used = budget;
        } else {
            omitted.push(if section.heading.is_empty() {
                "(preamble)"
            } else {
                &section.heading
            });
        }
    }

    // Restore document order for what survived: the customer wrote it in
    // an order that reads.
    kept.sort_by_key(|(order, _)| *order);
    let mut out = kept
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n\n");

    if !omitted.is_empty() {
        out.push_str(&format!(
            "\n\n[NOTE: {} intent section(s) did not fit and are NOT shown: {}. \
             You are deciding on partial intent — if the decision depends on \
             intent you cannot see, SKIP and say so.]",
            omitted.len(),
            omitted
                .iter()
                .map(|h| format!("\"{h}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

fn score(section: &IntentSection, terms: &[String]) -> i64 {
    let haystack = format!("{} {}", section.heading, section.body).to_ascii_lowercase();
    let overlap = terms.iter().filter(|t| haystack.contains(*t)).count() as i64;
    // Headings that read like rules prevent false positives even when they
    // share no vocabulary with this particular report.
    let heading = section.heading.to_ascii_lowercase();
    let rule_like = ["constraint", "invariant", "not a bug", "never", "do not"]
        .iter()
        .any(|k| heading.contains(k));
    overlap * 2 + if rule_like { 3 } else { 0 }
}

/// Character-budget truncation with a visible marker. Local to this module:
/// `merge0-triage` has its own copy, and context cannot depend on triage
/// (triage depends on context).
fn truncate_to(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(24).max(1);
    let kept: String = text.chars().take(keep).collect();
    format!("{kept}… [truncated to fit]")
}

#[cfg(test)]
mod retrieval_tests {
    use super::*;

    /// A doc long enough that a 2000-char budget cannot hold it, shaped like
    /// a real customer doc: the constraints and the fence sit last, which is
    /// exactly what head-truncation used to cut first.
    fn big_doc() -> String {
        let filler = "Background prose that is not about any particular \
                      defect but takes up room. "
            .repeat(20);
        format!(
            "# MERGE0.md\n\n{filler}\n\n\
             ## Architecture\n{filler}\n\n\
             ## Onboarding notes\n{filler}\n\n\
             ## Invariants\n- Schools may exist without a district.\n\n\
             ## Constraints for generated fixes\n\
             - Never change sync scheduling logic without design review.\n\n\
             {FENCE_START}\n- do not touch the district backfill job\n{FENCE_END}\n"
        )
    }

    #[test]
    fn sections_include_the_machine_fence_as_its_own_section() {
        let found = sections(&big_doc());
        let managed: Vec<_> = found.iter().filter(|s| s.managed).collect();
        assert_eq!(managed.len(), 1, "exactly one managed section");
        assert!(managed[0].body.contains("district backfill job"));
        assert!(found.iter().any(|s| s.heading == "Invariants"));
        assert!(found
            .iter()
            .any(|s| s.heading == "Constraints for generated fixes"));
    }

    /// The regression that motivated all of this: `merge0-hardening` writes
    /// its earned constraints into the fence, and the gate never saw them —
    /// first because the fence was stripped, then because the tail of a long
    /// doc was truncated away. A fenced amendment must survive both.
    #[test]
    fn a_fenced_amendment_survives_a_doc_far_over_budget() {
        let doc = big_doc();
        assert!(doc.len() > 2000, "fixture must exceed the budget");
        let out = relevant_intent(&doc, "sync scheduling fails for schools", 2000);
        assert!(out.len() <= 2400, "stays near budget: {}", out.len());
        assert!(
            out.contains("district backfill job"),
            "machine-managed amendment must reach the gate:\n{out}"
        );
    }

    #[test]
    fn omissions_are_disclosed_by_heading() {
        let out = relevant_intent(&big_doc(), "sync scheduling", 2000);
        assert!(out.contains("[NOTE:"), "must disclose:\n{out}");
        assert!(
            out.contains("\"Architecture\"") || out.contains("\"Onboarding notes\""),
            "omitted sections are named:\n{out}"
        );
        assert!(out.contains("SKIP"), "tells the gate what to do about it");
    }

    #[test]
    fn nothing_is_disclosed_when_everything_fits() {
        let out = relevant_intent(MERGE0_TEMPLATE, "anything at all", 100_000);
        assert!(!out.contains("[NOTE:"), "no false disclosure:\n{out}");
        assert!(out.contains("Invariants"));
        assert!(out.contains("Constraints for generated fixes"));
    }

    #[test]
    fn constraint_like_headings_outrank_unrelated_prose() {
        // Budget fits roughly two sections, and the focus text shares no
        // vocabulary with any of them — the rule-like heading still wins.
        let doc = format!(
            "## Chit chat\n{}\n\n## Constraints for generated fixes\n\
             - Never rewrite the billing exporter.\n\n{FENCE_START}\n{FENCE_END}\n",
            "words ".repeat(200)
        );
        let out = relevant_intent(&doc, "zzzz unrelated qqqq", 400);
        assert!(out.contains("billing exporter"), "\n{out}");
        assert!(
            out.contains("\"Chit chat\""),
            "and discloses the drop:\n{out}"
        );
    }

    #[test]
    fn selection_is_deterministic() {
        let doc = big_doc();
        let a = relevant_intent(&doc, "sync scheduling fails", 900);
        let b = relevant_intent(&doc, "sync scheduling fails", 900);
        assert_eq!(a, b);
    }

    #[test]
    fn a_single_oversized_section_still_yields_content() {
        let doc = format!("## Everything\n{}\n", "long ".repeat(500));
        let out = relevant_intent(&doc, "anything", 200);
        assert!(!out.trim().is_empty());
        assert!(out.contains("truncated to fit"), "\n{out}");
        assert!(out.len() < 400);
    }

    #[test]
    fn kept_sections_stay_in_document_order() {
        let out = relevant_intent(MERGE0_TEMPLATE, "school district onboarding", 100_000);
        let inv = out.find("## Invariants").expect("invariants kept");
        let con = out
            .find("## Constraints for generated fixes")
            .expect("constraints kept");
        assert!(inv < con, "customer's reading order preserved:\n{out}");
    }

    #[test]
    fn a_doc_without_a_fence_is_still_split() {
        let out = relevant_intent(
            "## Invariants\n- a rule\n\n## Notes\n- something else\n",
            "rule",
            100_000,
        );
        assert!(out.contains("- a rule"));
        assert!(out.contains("- something else"));
    }

    #[test]
    fn empty_intent_yields_empty_output() {
        assert_eq!(relevant_intent("", "focus", 2000), "");
        assert_eq!(relevant_intent("   \n\n", "focus", 2000), "");
    }
}
