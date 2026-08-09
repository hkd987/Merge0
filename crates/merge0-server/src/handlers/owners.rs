//! CODEOWNERS routing for reports: map the file paths a report's evidence
//! mentions to the owning teams/users from the product repo's CODEOWNERS.
//! Best-effort by design — a missing or unfetchable CODEOWNERS must never
//! fail the inbox, it just means no routing.

use crate::AppState;
use merge0_github::codeowners::{extract_paths, CodeOwners, CODEOWNERS_PATHS};
use merge0_signal::{Report, WorkOrder};

/// How many member signals contribute path evidence (a 500-signal cluster
/// does not need 500 fetches to find its file paths).
const MAX_SIGNALS_SCANNED: usize = 10;

/// `[{path, owners}]` for every evidence path with at least one owner,
/// or `None` when the repo has no CODEOWNERS (or it is unreachable).
pub async fn code_owners_for_report(
    state: &AppState,
    report: &Report,
    work_order: Option<&WorkOrder>,
) -> Option<Vec<serde_json::Value>> {
    let owners = fetch_codeowners(state).await?;

    let mut text = format!("{}\n{}", report.title, report.summary);
    if let Some(order) = work_order {
        text.push('\n');
        text.push_str(&order.summary);
        text.push('\n');
        text.push_str(&order.repro);
        if let Some(suspect) = &order.suspect_change {
            text.push('\n');
            text.push_str(suspect);
        }
    }
    for fingerprint in report.fingerprints.iter().take(MAX_SIGNALS_SCANNED) {
        if let Ok(Some(signal)) = state.tenant.signal_by_fingerprint(fingerprint).await {
            text.push('\n');
            text.push_str(&signal.title);
            text.push('\n');
            text.push_str(&signal.body);
        }
    }

    let routed: Vec<serde_json::Value> = extract_paths(&text)
        .into_iter()
        .filter_map(|path| {
            let path_owners = owners.owners_for(&path);
            (!path_owners.is_empty())
                .then(|| serde_json::json!({ "path": path, "owners": path_owners }))
        })
        .collect();
    Some(routed)
}

async fn fetch_codeowners(state: &AppState) -> Option<CodeOwners> {
    for path in CODEOWNERS_PATHS {
        match state.github.get_file_content(&state.repo, path).await {
            Ok(Some(content)) => {
                let parsed = CodeOwners::parse(&content);
                if !parsed.is_empty() {
                    return Some(parsed);
                }
            }
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("CODEOWNERS fetch failed at {path}: {e}");
                return None;
            }
        }
    }
    None
}
