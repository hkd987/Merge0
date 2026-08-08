//! Delivery targets: turning an approved Work Order into a tracker story.
//!
//! Merge0's default terminal action is a test-passing PR, but that asks a
//! team to trust autonomous code before it has any merge-rate history with
//! them. A tracker story is the same evidence-backed artifact delivered
//! where planning already happens — so the loop is useful on day one, and
//! PRs become an upgrade rather than a precondition.
//!
//! Two shapes, both driven by the same mapping:
//!
//! - **story** — the story *is* the delivery; no runner, no PR.
//! - **story_and_pr** — the story accompanies the dispatch, so the tracker
//!   reflects work the agent is already doing.
//!
//! Story text is assembled by [`story_from_work_order`], a pure function
//! with no I/O, so what a reviewer would read is unit-testable without a
//! tracker. [`Tracker`] is the only I/O boundary — mirroring `SlackSink`.
//!
//! **Anti-loop.** Every story is stamped with [`merge0_signal::ORIGIN_LABEL`]
//! and an `merge0:report <ulid>` line. Adapters ingesting the same tracker
//! skip anything carrying the label; without that pairing Merge0 would
//! re-ingest its own stories and triage itself forever.

use async_trait::async_trait;
use merge0_signal::{WorkOrder, ORIGIN_LABEL};

pub mod jira;
pub use jira::JiraTracker;

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("tracker request failed: {0}")]
    Transport(String),
    #[error("tracker rejected the story ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("tracker response was not the shape we expect: {0}")]
    Malformed(String),
}

/// A story to create, already rendered from a Work Order.
#[derive(Debug, Clone, PartialEq)]
pub struct Story {
    pub title: String,
    /// Plain text; vendor clients convert to their own markup.
    pub description: String,
    pub labels: Vec<String>,
}

/// What the tracker gave back.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedStory {
    /// Human-facing key, e.g. `ENG-1421`.
    pub key: String,
    /// Deep link a reviewer can open.
    pub url: String,
}

/// Anything that can file a story. One method — creation is the whole
/// contract; Merge0 never edits or transitions a human's tickets.
#[async_trait]
pub trait Tracker: Send + Sync {
    async fn create_story(&self, story: &Story) -> Result<CreatedStory, TrackerError>;
}

/// The origin line embedded in every story body.
///
/// Belt to the label's braces: labels are routinely stripped by tracker
/// automation, and if that happens the body still identifies the story as
/// Merge0's own — and points a human back at the report it came from.
pub fn origin_marker(work_order: &WorkOrder) -> String {
    format!("merge0:report {}", work_order.report_id)
}

/// Render an approved Work Order as story text.
///
/// The Work Order already carries exactly what a good story needs — that
/// is the point of the gate — so this is arrangement, not invention: no
/// field is summarized away, and nothing is added that the gate did not
/// stand behind.
pub fn story_from_work_order(work_order: &WorkOrder, evidence_limit: usize) -> Story {
    let mut body = String::new();

    body.push_str(&work_order.summary);
    body.push_str("\n\n## Reproduction\n");
    body.push_str(work_order.repro.trim());

    body.push_str("\n\n## Done when\n");
    body.push_str(work_order.success_criteria.trim());

    if !work_order.constraints.trim().is_empty() {
        body.push_str("\n\n## Constraints\n");
        body.push_str(work_order.constraints.trim());
    }

    if let Some(suspect) = work_order
        .suspect_change
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        body.push_str("\n\n## Suspect change\n");
        body.push_str(suspect.trim());
    }

    if !work_order.evidence.is_empty() {
        body.push_str("\n\n## Evidence\n");
        for link in work_order.evidence.iter().take(evidence_limit) {
            body.push_str(&format!("- {}: {}\n", link.label, link.url));
        }
        let hidden = work_order.evidence.len().saturating_sub(evidence_limit);
        if hidden > 0 {
            // Say so rather than silently truncating — a reviewer who
            // cannot see the count cannot know to go looking.
            body.push_str(&format!("- (+{hidden} more in the Merge0 inbox)\n"));
        }
    }

    if !work_order.prior_attempts.is_empty() {
        body.push_str(&format!(
            "\n\n## Prior attempts\n{} earlier attempt(s) recorded in outcome memory — \
             check the Merge0 report before re-doing work.\n",
            work_order.prior_attempts.len()
        ));
        // Whoever picks this story up is about to solve a problem someone
        // already tried to solve. A link to that attempt is worth more than
        // the count: it is the difference between a warning and a head start.
        for attempt in &work_order.prior_attempts {
            if let Some(url) = attempt.pr_url.as_deref() {
                body.push_str(&format!(
                    "- {} on {}: {url}\n",
                    serde_json::to_string(&attempt.outcome)
                        .unwrap_or_default()
                        .trim_matches('"'),
                    attempt.occurred_at.date_naive()
                ));
            }
        }
    }

    body.push_str(&format!(
        "\n\n---\nFiled by Merge0 from evidence. {}\n",
        origin_marker(work_order)
    ));

    Story {
        title: title_from(&work_order.summary),
        description: body,
        labels: vec![ORIGIN_LABEL.to_string()],
    }
}

/// Trackers reject or awkwardly render very long summaries, so the title is
/// the summary's first sentence, capped. The full summary always survives
/// as the first line of the description.
fn title_from(summary: &str) -> String {
    const MAX: usize = 160;
    let first = summary
        .split_once(". ")
        .map(|(head, _)| head)
        .unwrap_or(summary)
        .trim();
    let first = if first.is_empty() {
        summary.trim()
    } else {
        first
    };
    if first.chars().count() <= MAX {
        return first.to_string();
    }
    let truncated: String = first.chars().take(MAX - 1).collect();
    format!("{}…", truncated.trim_end())
}

/// Records stories in memory instead of filing them (tests, dry-run, and
/// `MERGE0_DEV_FAKES=1`).
#[derive(Default)]
pub struct RecordingTracker {
    stories: std::sync::Mutex<Vec<Story>>,
    /// When set, every call fails — exercises the failure semantics that
    /// differ between `story` and `story_and_pr` mode.
    fail_with: Option<String>,
}

impl RecordingTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            stories: std::sync::Mutex::new(Vec::new()),
            fail_with: Some(message.into()),
        }
    }

    pub fn stories(&self) -> Vec<Story> {
        self.stories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Tracker for RecordingTracker {
    async fn create_story(&self, story: &Story) -> Result<CreatedStory, TrackerError> {
        if let Some(message) = &self.fail_with {
            return Err(TrackerError::Transport(message.clone()));
        }
        let mut stories = self
            .stories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stories.push(story.clone());
        let n = stories.len();
        Ok(CreatedStory {
            key: format!("FAKE-{n}"),
            url: format!("https://tracker.example.com/browse/FAKE-{n}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merge0_signal::{
        DiffBudget, EvidenceKind, EvidenceLink, GateConfidence, OutcomeKind, OutcomeRef,
    };
    use ulid::Ulid;

    fn work_order() -> WorkOrder {
        WorkOrder {
            report_id: Ulid::new(),
            repo: "chalk/chalk".into(),
            summary: "Attendance export drops the last student. Seen on 26 rosters.".into(),
            evidence: vec![
                EvidenceLink {
                    kind: EvidenceKind::Issue,
                    label: "Sentry issue CHALK-9".into(),
                    url: "https://sentry.example.com/1".into(),
                },
                EvidenceLink {
                    kind: EvidenceKind::Ticket,
                    label: "Zendesk 7719".into(),
                    url: "https://chalk.zendesk.example.com/7719".into(),
                },
            ],
            repro: "Export a class of exactly 25 students from /classes/roster.".into(),
            suspect_change: Some("regressed in v2.3.0".into()),
            success_criteria: "A regression test covers the boundary row.".into(),
            constraints: "Do not change sync scheduling.".into(),
            prior_attempts: vec![],
            diff_budget: DiffBudget::default(),
            confidence: GateConfidence::High,
        }
    }

    #[test]
    fn story_carries_every_work_order_field_a_reviewer_needs() {
        let order = work_order();
        let story = story_from_work_order(&order, 8);

        assert!(story.description.contains("Attendance export drops"));
        assert!(story.description.contains("/classes/roster"));
        assert!(story.description.contains("regression test covers"));
        assert!(story.description.contains("Do not change sync scheduling"));
        assert!(story.description.contains("regressed in v2.3.0"));
        assert!(story.description.contains("https://sentry.example.com/1"));
        assert!(story.description.contains("Zendesk 7719"));
    }

    /// The anti-loop stamp is the whole reason ingesting our own tracker is
    /// safe, so it is asserted in both places it lives.
    #[test]
    fn every_story_is_stamped_as_merge0_origin() {
        let order = work_order();
        let story = story_from_work_order(&order, 8);
        assert!(story.labels.contains(&ORIGIN_LABEL.to_string()));
        assert!(story
            .description
            .contains(&format!("merge0:report {}", order.report_id)));
    }

    #[test]
    fn title_is_the_first_sentence_and_stays_within_tracker_limits() {
        let mut order = work_order();
        let story = story_from_work_order(&order, 8);
        assert_eq!(story.title, "Attendance export drops the last student");

        order.summary = "x".repeat(400);
        let story = story_from_work_order(&order, 8);
        assert!(story.title.chars().count() <= 160, "title must be capped");
        assert!(story.title.ends_with('…'));
        // The full text is never lost — only the title is shortened.
        assert!(story.description.contains(&"x".repeat(400)));
    }

    #[test]
    fn over_budget_evidence_is_disclosed_not_silently_dropped() {
        let mut order = work_order();
        order.evidence = (0..10)
            .map(|i| EvidenceLink {
                kind: EvidenceKind::Issue,
                label: format!("link {i}"),
                url: format!("https://tool.example.com/{i}"),
            })
            .collect();
        let story = story_from_work_order(&order, 3);
        assert!(story.description.contains("link 0"));
        assert!(!story.description.contains("link 9"));
        assert!(story.description.contains("(+7 more"));
    }

    #[test]
    fn prior_attempts_warn_the_reader_before_they_redo_the_work() {
        let mut order = work_order();
        order.prior_attempts = vec![OutcomeRef {
            work_order_id: Ulid::new(),
            outcome: OutcomeKind::Reverted,
            occurred_at: chrono::Utc::now(),
            note: Some("broke admin view".into()),
            pr_url: Some("https://github.com/chalk/chalk/pull/412".into()),
        }];
        let story = story_from_work_order(&order, 8);
        assert!(story.description.contains("Prior attempts"));
        assert!(story.description.contains("1 earlier attempt"));
        // The link, not just the count: whoever picks this up should be able
        // to read what was tried before they try it again.
        assert!(story
            .description
            .contains("https://github.com/chalk/chalk/pull/412"));
    }

    #[tokio::test]
    async fn recording_tracker_captures_and_can_fail_on_demand() {
        let tracker = RecordingTracker::new();
        let created = tracker
            .create_story(&story_from_work_order(&work_order(), 8))
            .await
            .unwrap();
        assert_eq!(created.key, "FAKE-1");
        assert_eq!(tracker.stories().len(), 1);

        let broken = RecordingTracker::failing("jira is down");
        let err = broken
            .create_story(&story_from_work_order(&work_order(), 8))
            .await
            .expect_err("must surface the failure");
        assert!(err.to_string().contains("jira is down"));
    }
}
