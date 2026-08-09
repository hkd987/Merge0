//! `GET /metrics` — the telemetry snapshot in Prometheus text exposition
//! format, hand-rendered (no client library: a handful of gauges does not
//! justify a dependency). Mounted on the PROTECTED router — configure the
//! scraper with `authorization: Bearer <MERGE0_API_TOKEN>`.

use super::ApiError;
use crate::AppState;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use merge0_signal::ReportStatus;

const WINDOW_DAYS: u32 = 30;

pub async fn scrape(State(state): State<AppState>) -> Result<Response, ApiError> {
    let snapshot = state
        .tenant
        .telemetry(WINDOW_DAYS, state.efficacy_grace_days, Utc::now())
        .await?;
    let mut out = String::with_capacity(2048);

    let mut gauge = |name: &str, help: &str, value: f64| {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
        ));
    };

    let c = &snapshot.counts;
    gauge(
        "merge0_window_days",
        "Window (days) the merge0_* series are computed over.",
        f64::from(c.window_days),
    );
    gauge(
        "merge0_work_orders_dispatched",
        "Work Orders dispatched to customer-side compute in the window.",
        c.dispatched as f64,
    );
    gauge(
        "merge0_prs_opened",
        "Test-passing PRs opened in the window.",
        c.prs_opened as f64,
    );
    gauge(
        "merge0_prs_merged",
        "Merge0 PRs merged in the window.",
        c.prs_merged as f64,
    );
    gauge(
        "merge0_prs_closed",
        "Merge0 PRs closed unmerged in the window.",
        c.prs_closed as f64,
    );
    gauge(
        "merge0_prs_reverted",
        "Merged Merge0 PRs reverted within the revert window.",
        c.prs_reverted as f64,
    );
    gauge(
        "merge0_runs_discarded",
        "Runs that self-discarded (budgets exhausted) in the window.",
        c.runs_discarded as f64,
    );
    if let Some(rate) = snapshot.merge_rate {
        gauge(
            "merge0_merge_rate",
            "merged / decided PRs in the window (the Phase 0 gate metric).",
            rate,
        );
    }
    if let Some(rate) = snapshot.runner_yield {
        gauge(
            "merge0_runner_yield",
            "PRs opened / Work Orders dispatched.",
            rate,
        );
    }
    if let Some(rate) = snapshot.gate_precision {
        gauge(
            "merge0_gate_precision",
            "approved / (approved + dismissed) reports.",
            rate,
        );
    }
    if let Some(tokens) = snapshot.tokens_per_merged_pr {
        gauge(
            "merge0_tokens_per_merged_pr",
            "Mean tokens spent per merged PR.",
            tokens,
        );
    }
    gauge(
        "merge0_phase0_gate_met",
        "1 when the Phase 0 validation gate (>=10 decided, >=60% merged) holds.",
        f64::from(u8::from(snapshot.phase0_gate_met)),
    );
    gauge(
        "merge0_fixes_confirmed",
        "Merged fixes whose signals stayed quiet past the grace period.",
        c.fixes_confirmed as f64,
    );
    gauge(
        "merge0_fixes_recurred",
        "Merged fixes whose member signals recurred after the grace period.",
        c.fixes_recurred as f64,
    );
    if let Some(rate) = snapshot.fix_efficacy_rate {
        gauge(
            "merge0_fix_efficacy_rate",
            "confirmed / (confirmed + recurred) merged fixes.",
            rate,
        );
    }
    gauge(
        "merge0_auto_dispatched",
        "Dispatches pulled by the autonomy dial (not a human) in the window.",
        c.auto_dispatched as f64,
    );
    gauge(
        "merge0_tokens_spent_24h",
        "Model tokens spent in the trailing 24h (gate + runner).",
        c.tokens_spent_24h as f64,
    );
    let budget = state.gate.budget.max_tokens_per_day;
    if budget > 0 {
        gauge(
            "merge0_token_budget_remaining",
            "Tokens left in the rolling 24h budget (0 = gate paused).",
            budget.saturating_sub(c.tokens_spent_24h) as f64,
        );
    }

    // Loop liveness: when triage last ran. Alert on staleness (now - this
    // exceeding the configured interval) — a wedged scheduler is otherwise
    // invisible because every count above simply stops moving. Omitted
    // (not 0) before the first run so "never ran" can't hide as 1970.
    if let Some(at) = state.tenant.last_triage_run_at().await? {
        gauge(
            "merge0_last_triage_run_timestamp_seconds",
            "Unix time the most recent triage run started.",
            at.timestamp() as f64,
        );
    }

    // Per-source fetch freshness (from the persisted cursor table, so it
    // survives restarts) + in-process failure counters.
    let fetch_runs = state.tenant.fetch_last_runs().await?;
    if !fetch_runs.is_empty() {
        out.push_str(
            "# HELP merge0_fetch_last_run_timestamp_seconds Unix time the source's poller last completed.\n\
             # TYPE merge0_fetch_last_run_timestamp_seconds gauge\n",
        );
        for (source, at) in &fetch_runs {
            out.push_str(&format!(
                "merge0_fetch_last_run_timestamp_seconds{{source=\"{source}\"}} {}\n",
                at.timestamp()
            ));
        }
    }
    if !state.fetchers.is_empty()
        || !state
            .fetch_failures
            .lock()
            .expect("not poisoned")
            .is_empty()
    {
        out.push_str(
            "# HELP merge0_fetch_failures_total Failed poll rounds per source since process start.\n\
             # TYPE merge0_fetch_failures_total counter\n",
        );
        // Zero-series for every enabled source so increase() has a
        // baseline; recorded failures override.
        let failures = state.fetch_failures.lock().expect("not poisoned").clone();
        let mut sources: Vec<String> = state
            .fetchers
            .iter()
            .map(|f| f.source_name().to_string())
            .chain(failures.keys().cloned())
            .collect();
        sources.sort();
        sources.dedup();
        for source in sources {
            let count = failures.get(&source).copied().unwrap_or(0);
            out.push_str(&format!(
                "merge0_fetch_failures_total{{source=\"{source}\"}} {count}\n"
            ));
        }
    }

    // Live queue depths by report status (labels, one TYPE header).
    out.push_str(
        "# HELP merge0_reports Current report count by lifecycle status.\n\
         # TYPE merge0_reports gauge\n",
    );
    for (status, label) in [
        (ReportStatus::Pending, "pending"),
        (ReportStatus::AwaitingReview, "awaiting_review"),
        (ReportStatus::PrOpen, "pr_open"),
        (ReportStatus::Dispatched, "dispatched"),
    ] {
        let count = state.tenant.list_reports(Some(status)).await?.len();
        out.push_str(&format!("merge0_reports{{status=\"{label}\"}} {count}\n"));
    }

    Ok((
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        out,
    )
        .into_response())
}
