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
