//! Pure Block Kit message builders (PRD §6a item 2).
//!
//! Each builder takes exactly one report (or one report + one PR): the
//! one-report-per-message invariant is enforced by the type signatures.
//! The deep link into the inbox is appended as a context block — a fallback
//! for edge cases, never required to reach a decision.

use merge0_signal::{DismissReason, EvidenceLink, Report, Severity};
use serde_json::{json, Value};

/// `action_id` of the inline approve button.
pub const ACTION_APPROVE: &str = "approve";
/// `action_id` of the dismiss reason select.
pub const ACTION_DISMISS: &str = "dismiss";
/// PRs pending longer than this show up flagged in the weekly digest
/// (reviewer-abandonment mitigation, PRD Risks table).
pub const STALE_PR_AGE_DAYS: i64 = 7;

/// The four structured dismissal reasons, in the order they are offered.
pub const DISMISS_REASONS: [DismissReason; 4] = [
    DismissReason::IntendedBehavior,
    DismissReason::WontFix,
    DismissReason::Duplicate,
    DismissReason::BadEvidence,
];

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
    }
}

fn dismiss_label(reason: DismissReason) -> &'static str {
    match reason {
        DismissReason::IntendedBehavior => "Intended behavior",
        DismissReason::WontFix => "Won't fix",
        DismissReason::Duplicate => "Duplicate",
        DismissReason::BadEvidence => "Bad evidence",
    }
}

/// One-line evidence summary: first link's label plus a count of the rest.
fn evidence_summary(evidence: &[EvidenceLink]) -> String {
    match evidence {
        [] => "no evidence links".to_string(),
        [only] => only.label.clone(),
        [first, rest @ ..] => format!("{} (+{} more)", first.label, rest.len()),
    }
}

fn affected_label(report: &Report) -> String {
    report
        .affected_count
        .map_or_else(|| "unknown".to_string(), |n| n.to_string())
}

/// New-report notification: decision-ready in one message.
///
/// Carries severity, affected count, and a one-line evidence summary inline,
/// an approve button (`action_id = "approve"`, `value = <report id>`), and a
/// dismiss select whose options are the four structured
/// [`DismissReason`] values (option value `"<report id>:<reason>"` so the
/// interaction payload is self-contained). The `{inbox_url}/reports/{id}`
/// deep link rides along in a trailing context block as fallback only.
pub fn report_message(report: &Report, inbox_url: &str) -> Value {
    let id = report.id.to_string();
    let deep_link = format!("{}/reports/{id}", inbox_url.trim_end_matches('/'));
    let dismiss_options: Vec<Value> = DISMISS_REASONS
        .iter()
        .map(|reason| {
            json!({
                "text": { "type": "plain_text", "text": dismiss_label(*reason) },
                "value": format!("{id}:{}", reason.as_str()),
            })
        })
        .collect();

    json!({
        "text": format!(
            "[{}] {} — affected: {}",
            severity_label(report.severity),
            report.title,
            affected_label(report),
        ),
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("*{}*\n{}", report.title, report.summary),
                },
            },
            {
                "type": "section",
                "fields": [
                    {
                        "type": "mrkdwn",
                        "text": format!("*Severity*\n{}", severity_label(report.severity)),
                    },
                    {
                        "type": "mrkdwn",
                        "text": format!("*Affected*\n{}", affected_label(report)),
                    },
                    {
                        "type": "mrkdwn",
                        "text": format!("*Evidence*\n{}", evidence_summary(&report.evidence)),
                    },
                ],
            },
            {
                "type": "actions",
                "block_id": id.clone(),
                "elements": [
                    {
                        "type": "button",
                        "action_id": ACTION_APPROVE,
                        "style": "primary",
                        "value": id.clone(),
                        "text": { "type": "plain_text", "text": "Approve" },
                    },
                    {
                        "type": "static_select",
                        "action_id": ACTION_DISMISS,
                        "placeholder": { "type": "plain_text", "text": "Dismiss (reason)" },
                        "options": dismiss_options,
                    },
                ],
            },
            {
                "type": "context",
                "elements": [
                    {
                        "type": "mrkdwn",
                        "text": format!("<{deep_link}|Open in inbox> — fallback; the decision is available inline"),
                    },
                ],
            },
        ],
    })
}

/// PR-ready notification: one PR per message, link to review.
pub fn pr_ready_message(report: &Report, pr_url: &str) -> Value {
    json!({
        "text": format!("PR ready for review: {}", report.title),
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "*PR ready for review*\n*{}*\nTests passing; awaiting human merge.",
                        report.title,
                    ),
                },
            },
            {
                "type": "actions",
                "elements": [
                    {
                        "type": "button",
                        "action_id": "open_pr",
                        "url": pr_url,
                        "text": { "type": "plain_text", "text": "Review PR" },
                    },
                ],
            },
            {
                "type": "context",
                "elements": [
                    { "type": "mrkdwn", "text": format!("Report {}", report.id) },
                ],
            },
        ],
    })
}

/// Weekly digest: pending reports, PRs awaiting merge with aging, merged
/// count. Anything open longer than [`STALE_PR_AGE_DAYS`] is flagged
/// `[STALE]` so nothing rots silently (PRD reviewer-abandonment risk).
///
/// `prs_awaiting` pairs are `(pr_url, age_days)`; ages are computed by the
/// caller so this stays a pure function with no wall-clock reads.
pub fn weekly_digest(
    pending: &[Report],
    prs_awaiting: &[(String, i64)],
    merged_this_week: u64,
) -> Value {
    let pending_lines = if pending.is_empty() {
        "none".to_string()
    } else {
        pending
            .iter()
            .map(|r| format!("- [{}] {}", severity_label(r.severity), r.title))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let pr_lines = if prs_awaiting.is_empty() {
        "none".to_string()
    } else {
        prs_awaiting
            .iter()
            .map(|(url, age_days)| {
                if *age_days > STALE_PR_AGE_DAYS {
                    format!("- <{url}> — open {age_days} days [STALE]")
                } else {
                    format!("- <{url}> — open {age_days} days")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    json!({
        "text": format!(
            "Merge0 weekly digest: {merged_this_week} merged, {} awaiting review, {} PRs awaiting merge",
            pending.len(),
            prs_awaiting.len(),
        ),
        "blocks": [
            {
                "type": "header",
                "text": { "type": "plain_text", "text": "Merge0 weekly digest" },
            },
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "*{merged_this_week}* merged this week - *{}* reports awaiting review - *{}* PRs awaiting merge",
                        pending.len(),
                        prs_awaiting.len(),
                    ),
                },
            },
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("*Reports awaiting review*\n{pending_lines}"),
                },
            },
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("*PRs awaiting merge*\n{pr_lines}"),
                },
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use merge0_signal::{EvidenceKind, ReportKind, ReportStatus};
    use ulid::Ulid;

    fn fixture_report() -> Report {
        Report {
            id: Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            kind: ReportKind::Maintenance,
            title: "Null district crash in SyncStatusPanel".into(),
            summary: "42 users since v2.3.0, corroborated by rage clicks".into(),
            severity: Severity::High,
            evidence: vec![
                EvidenceLink {
                    kind: EvidenceKind::StackTrace,
                    label: "Sentry stack trace".into(),
                    url: "https://sentry.example.com/issues/5312345678".into(),
                },
                EvidenceLink {
                    kind: EvidenceKind::Replay,
                    label: "PostHog replay".into(),
                    url: "https://posthog.example.com/replay/abc".into(),
                },
            ],
            signal_ids: vec![Ulid::new()],
            fingerprints: vec!["sentry:abc".into()],
            suspect_release: Some("v2.3.0".into()),
            affected_count: Some(42),
            status: ReportStatus::AwaitingReview,
            created_at: Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Collect every `action_id` present in any actions block.
    fn action_ids(message: &Value) -> Vec<String> {
        let mut ids = Vec::new();
        for block in message["blocks"].as_array().unwrap() {
            if block["type"] == "actions" {
                for element in block["elements"].as_array().unwrap() {
                    ids.push(element["action_id"].as_str().unwrap().to_string());
                }
            }
        }
        ids
    }

    #[test]
    fn report_message_is_decision_ready() {
        let report = fixture_report();
        let msg = report_message(&report, "https://inbox.example.com");
        let fields = msg["blocks"][1]["fields"].as_array().unwrap();
        assert!(fields[0]["text"].as_str().unwrap().contains("high"));
        assert!(fields[1]["text"].as_str().unwrap().contains("42"));
        assert!(fields[2]["text"]
            .as_str()
            .unwrap()
            .contains("Sentry stack trace (+1 more)"));
    }

    #[test]
    fn report_message_has_approve_and_structured_dismiss() {
        let report = fixture_report();
        let msg = report_message(&report, "https://inbox.example.com");
        let actions = &msg["blocks"][2];
        assert_eq!(actions["type"], "actions");
        let approve = &actions["elements"][0];
        assert_eq!(approve["action_id"], "approve");
        assert_eq!(approve["value"], report.id.to_string());

        let dismiss = &actions["elements"][1];
        assert_eq!(dismiss["action_id"], "dismiss");
        let options = dismiss["options"].as_array().unwrap();
        assert_eq!(options.len(), 4);
        for (option, reason) in options.iter().zip(DISMISS_REASONS) {
            assert_eq!(
                option["value"].as_str().unwrap(),
                format!("{}:{}", report.id, reason.as_str())
            );
        }
    }

    #[test]
    fn report_message_deep_link_is_fallback_context() {
        let report = fixture_report();
        // Trailing slash on the inbox URL must not produce a double slash.
        let msg = report_message(&report, "https://inbox.example.com/");
        let context = &msg["blocks"][3];
        assert_eq!(context["type"], "context");
        let text = context["elements"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("https://inbox.example.com/reports/{}", report.id)));
        assert!(text.contains("fallback"));
    }

    #[test]
    fn one_report_per_message_invariant() {
        // The builders take a single report by construction; assert the
        // rendered message carries exactly one approve action and one
        // dismiss select referencing exactly one report id.
        let report = fixture_report();
        let msg = report_message(&report, "https://inbox.example.com");
        let ids = action_ids(&msg);
        assert_eq!(
            ids.iter().filter(|id| *id == "approve").count(),
            1,
            "exactly one approve action per message"
        );
        assert_eq!(ids.iter().filter(|id| *id == "dismiss").count(), 1);
    }

    #[test]
    fn pr_ready_message_links_the_pr() {
        let report = fixture_report();
        let msg = pr_ready_message(&report, "https://github.example.com/chalk/chalk/pull/99");
        let button = &msg["blocks"][1]["elements"][0];
        assert_eq!(
            button["url"],
            "https://github.example.com/chalk/chalk/pull/99"
        );
        assert!(msg["text"].as_str().unwrap().contains(&report.title));
    }

    #[test]
    fn weekly_digest_flags_stale_prs_only() {
        let prs = vec![
            (
                "https://github.example.com/chalk/chalk/pull/1".to_string(),
                9,
            ),
            (
                "https://github.example.com/chalk/chalk/pull/2".to_string(),
                3,
            ),
        ];
        let msg = weekly_digest(&[], &prs, 5);
        let pr_section = msg["blocks"][3]["text"]["text"].as_str().unwrap();
        let lines: Vec<&str> = pr_section.lines().collect();
        assert!(lines[1].contains("pull/1"));
        assert!(lines[1].contains("[STALE]"), "9-day PR must be flagged");
        assert!(lines[2].contains("pull/2"));
        assert!(
            !lines[2].contains("[STALE]"),
            "3-day PR must not be flagged"
        );
    }

    #[test]
    fn weekly_digest_summarizes_counts() {
        let pending = vec![fixture_report()];
        let msg = weekly_digest(&pending, &[], 7);
        let summary = msg["blocks"][1]["text"]["text"].as_str().unwrap();
        assert!(summary.contains("*7* merged this week"));
        assert!(summary.contains("*1* reports awaiting review"));
        assert!(summary.contains("*0* PRs awaiting merge"));
        let pending_section = msg["blocks"][2]["text"]["text"].as_str().unwrap();
        assert!(pending_section.contains("[high] Null district crash"));
    }
}
