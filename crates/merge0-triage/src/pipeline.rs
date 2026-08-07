//! The triage run: scouts → clustering → gate, end to end against the store.

use crate::cluster::{assemble_report, classify, cluster_signals, handoff_brief, Cluster};
use crate::config::{GateConfig, ScoutConfig};
use crate::{gate, scouts, Result};
use chrono::{DateTime, Duration, Utc};
use merge0_context::{outcome::prior_attempts, release::suspect_release};
use merge0_model::Model;
use merge0_signal::{DismissReason, GateDecision, ReportKind, Signal};
use merge0_store::TenantStore;

/// One triage run's summary — this is itself meta-loop telemetry.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct TriageRun {
    pub candidates: usize,
    pub reports_created: usize,
    pub work_orders: usize,
    pub skips: usize,
    pub opportunities: usize,
    pub tokens_used: u64,
}

/// Run the full triage pass for one tenant/repo.
///
/// `intent_text` is the customer's MERGE0.md human prose (already
/// fence-stripped by the caller via `merge0_context::intent`).
pub async fn run_triage(
    store: &TenantStore,
    model: &dyn Model,
    scout_configs: &[ScoutConfig],
    gate_config: &GateConfig,
    intent_text: &str,
    repo: &str,
    now: DateTime<Utc>,
) -> Result<TriageRun> {
    let mut run = TriageRun::default();

    // Scout stage: recent unassigned signals, selected per scout config.
    let widest_window = scout_configs
        .iter()
        .map(|s| scouts::schedule_window(&s.schedule))
        .max()
        .unwrap_or_else(|| Duration::hours(24));
    let unassigned = store.unassigned_signals().await?;
    let recent: Vec<Signal> = unassigned
        .into_iter()
        .filter(|s| s.last_seen >= now - widest_window)
        .collect();
    let candidates: Vec<Signal> = scouts::union_candidates(scout_configs, &recent, now)?
        .into_iter()
        .cloned()
        .collect();
    run.candidates = candidates.len();

    // Clustering + classification + report assembly.
    let timeline = store.releases().await?;
    let clusters: Vec<Cluster> = cluster_signals(candidates);
    for cluster in &clusters {
        // Recurrence history for the Opportunity classifier: by fingerprint
        // (same defect identity) or by url_path (same design collision under
        // a fresh fingerprint — new ticket, new session).
        let mut intended_history = false;
        for signal in &cluster.signals {
            if store
                .fingerprint_dismissed_as(&signal.fingerprint, DismissReason::IntendedBehavior)
                .await?
            {
                intended_history = true;
                break;
            }
            if let Some(path) = signal.join_keys.url_path.as_deref() {
                if store
                    .url_path_dismissed_as(path, DismissReason::IntendedBehavior)
                    .await?
                {
                    intended_history = true;
                    break;
                }
            }
        }
        let kind = classify(&cluster.signals, intended_history);
        let suspect = suspect_release(&cluster.signals, &timeline);
        let report = assemble_report(cluster, kind, suspect, gate_config, now);
        store.insert_report(&report).await?;
        run.reports_created += 1;

        if kind == ReportKind::Opportunity {
            // Terminal action is human handoff — no gate, no Work Order,
            // no PR (PRD P2, Opportunity Reports).
            let brief = handoff_brief(&report, &cluster.signals);
            store.hand_off_report(report.id, &brief, now).await?;
            run.opportunities += 1;
        }
    }

    // Gate stage: every pending maintenance report — this run's plus any
    // overflow left pending by earlier capped runs. Highest severity first.
    let mut maintenance_reports: Vec<_> = store
        .list_reports(Some(merge0_signal::ReportStatus::Pending))
        .await?
        .into_iter()
        .filter(|r| r.kind != ReportKind::Opportunity)
        .collect();
    maintenance_reports.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.affected_count.cmp(&a.affected_count))
    });
    for report in &maintenance_reports {
        if run.work_orders >= gate_config.max_work_orders_per_run as usize {
            break; // Remaining reports stay Pending for the next run.
        }
        let prior =
            prior_attempts(store, &report.fingerprints, gate_config.prior_attempts_cap).await?;
        let outcome = gate::evaluate(report, repo, intent_text, prior, gate_config, model).await?;
        run.tokens_used += outcome.tokens_used;
        match &outcome.decision {
            GateDecision::Work { .. } => run.work_orders += 1,
            GateDecision::Skip { .. } => run.skips += 1,
        }
        store
            .set_gate_decision(report.id, &outcome.decision)
            .await?;
    }

    Ok(run)
}
