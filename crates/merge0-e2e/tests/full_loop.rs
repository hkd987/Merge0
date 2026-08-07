//! The whole PRD in one test file: vendor payloads → Signals → Report →
//! gate → Work Order → dispatch → PR → merge → outcome memory → hardening
//! pass → meta-loop, plus the P2 broker and registry flows. Real Postgres;
//! fakes for the model, GitHub, and time-sensitive externals.

use chrono::{DateTime, Duration, TimeZone, Utc};
use merge0_adapters::Adapter;
use merge0_github::{FakeGitHub, GitHubApi, RepoRef};
use merge0_model::ScriptedModel;
use merge0_runner::{enforce_budgets, ActionsRunner, RunReport, RunStatus, Runner};
use merge0_signal::{OutcomeKind, ReportKind, ReportStatus, Severity};
use merge0_store::{Store, TenantStore};
use merge0_triage::pipeline::run_triage;
use std::sync::Arc;
use ulid::Ulid;

fn database_url() -> String {
    std::env::var("MERGE0_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://merge0@localhost:55432/merge0".to_string())
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 7, 6, 0, 0).unwrap()
}

async fn fresh_tenant() -> (Store, TenantStore, String) {
    let store = Store::connect(&database_url()).await.unwrap();
    let schema = format!("t_{}", Ulid::new().to_string().to_lowercase());
    let tenant = store.tenant(&schema).await.unwrap();
    (store, tenant, schema)
}

fn scouts() -> Vec<merge0_triage::config::ScoutConfig> {
    merge0_triage::config::load_scouts(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/scouts")
            .as_path(),
    )
    .expect("repo scout configs load")
}

fn gate_config() -> merge0_triage::config::GateConfig {
    merge0_triage::config::load_gate(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/gate.toml")
            .as_path(),
    )
    .expect("repo gate config loads")
}

/// A Sentry issue and a PostHog error-tracking issue for the SAME defect
/// (`TypeError`), as the vendors would ship them — normalized through the
/// real adapters, exactly like `POST /ingest/{source}`.
fn ingest_envelopes(last_seen: DateTime<Utc>) -> Vec<(&'static str, serde_json::Value)> {
    let last_seen = last_seen.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    vec![
        (
            "sentry",
            serde_json::json!({
                "endpoint": "issues",
                "payload": [{
                    "id": "9001",
                    "shortId": "CHALK-9",
                    "title": "TypeError: Cannot read properties of undefined (reading 'districtId')",
                    "permalink": "https://sentry.example.com/organizations/chalk/issues/9001/",
                    "level": "error",
                    "metadata": {"type": "TypeError", "value": "districtId undefined"},
                    "userCount": 33,
                    "firstSeen": "2026-08-06T04:00:00Z",
                    "lastSeen": last_seen,
                    "firstRelease": {"version": "v2.3.0"},
                }]
            }),
        ),
        (
            "posthog",
            serde_json::json!({
                "endpoint": "error_tracking_issues",
                "context": {"project_base_url": "https://us.posthog.com/project/1"},
                "payload": {"results": [{
                    "id": "ph-9001",
                    "name": "TypeError",
                    "description": "Cannot read properties of undefined (reading 'districtId')",
                    "first_seen": "2026-08-06T05:00:00Z",
                    "last_seen": last_seen,
                    "users": 21,
                }]}
            }),
        ),
    ]
}

async fn ingest(tenant: &TenantStore, last_seen: DateTime<Utc>) {
    for (source, envelope) in ingest_envelopes(last_seen) {
        let adapter: Box<dyn Adapter> = match source {
            "sentry" => Box::new(merge0_adapter_sentry::SentryAdapter),
            "posthog" => Box::new(merge0_adapter_posthog::PosthogAdapter),
            _ => unreachable!(),
        };
        for signal in adapter.normalize(&envelope).unwrap() {
            tenant.upsert_signal(&signal).await.unwrap();
        }
    }
}

const WORK_JSON: &str = r#"{"decision":"work","summary":"Handle schools with no linked district",
    "repro":"open /districts/sync for an unlinked school",
    "success_criteria":"SyncStatusPanel renders the empty state; regression test passes",
    "constraints":"do not touch sync scheduling"}"#;

#[tokio::test]
async fn the_complete_loop_fix_harden_meta() {
    let (store, tenant, schema) = fresh_tenant().await;
    let repo = RepoRef::parse("chalk/chalk").unwrap();
    let github = Arc::new(FakeGitHub::new());
    let model = ScriptedModel::new([WORK_JSON]);

    // ---- Ingest (P0-1/P0-2) + release context (P0-4) ----
    ingest(&tenant, now() - Duration::hours(1)).await;
    tenant
        .upsert_release("v2.3.0", Some("cafe23"), now() - Duration::days(6), None)
        .await
        .unwrap();

    // ---- Triage: cross-source cluster → gate → Work Order (P0-3, P0-5) ----
    let run = run_triage(
        &tenant,
        &model,
        &scouts(),
        &gate_config(),
        "Schools may exist without districts during onboarding.",
        "chalk/chalk",
        now(),
    )
    .await
    .unwrap();
    assert_eq!(
        run.reports_created, 1,
        "one report for one defect across two sources"
    );
    assert_eq!(run.work_orders, 1);

    let report = &tenant
        .list_reports(Some(ReportStatus::AwaitingReview))
        .await
        .unwrap()[0];
    assert_eq!(report.signal_ids.len(), 2, "P0-3: both signals, one report");
    assert_eq!(report.suspect_release.as_deref(), Some("v2.3.0"), "P0-4");
    let order = tenant.work_order(report.id).await.unwrap().unwrap();
    assert!(!order.success_criteria.is_empty(), "P0-5");

    // ---- Approve → dispatch (P0-6) ----
    tenant.approve_report(report.id, now()).await.unwrap();
    let runner = ActionsRunner {
        api: github.clone(),
        agent: merge0_runner::AgentKind::ClaudeCode,
        callback_url: "https://merge0.example.com/runner/callback".into(),
        attribution: None,
    };
    let receipt = runner.dispatch(&order).await.unwrap();
    tenant
        .record_dispatch(report.id, &receipt.runner_kind, now())
        .await
        .unwrap();

    // ---- Runner reports a test-passing PR within budget ----
    let callback = enforce_budgets(
        &order,
        RunReport {
            report_id: report.id.to_string(),
            status: RunStatus::Opened,
            pr_url: Some("https://github.com/chalk/chalk/pull/300".into()),
            branch: Some("merge0/fix".into()),
            discard_reason: None,
            diagnosis: None,
            tokens_spent: Some(95_000),
            files_changed: Some(2),
            total_lines_changed: Some(41),
            extensions: Some(serde_json::json!({"skills": []})),
        },
    );
    assert_eq!(callback.status, RunStatus::Opened);
    tenant
        .record_pr_opened(
            report.id,
            callback.pr_url.as_deref().unwrap(),
            callback.branch.as_deref().unwrap(),
            now() + Duration::minutes(20),
            callback.tokens_spent,
            callback.extensions.clone(),
        )
        .await
        .unwrap();

    // ---- Human merges; webhook event → outcome memory (P0-8) ----
    let merged_at = now() + Duration::hours(3);
    let event = merge0_github::webhook::parse(
        "pull_request",
        &serde_json::json!({
            "action": "closed",
            "pull_request": {
                "html_url": "https://github.com/chalk/chalk/pull/300",
                "merged": true,
                "merged_at": merged_at.to_rfc3339(),
                "merge_commit_sha": "abcd1234deadbeef",
                "title": "Handle schools with no linked district",
                "body": "",
            }
        }),
    )
    .unwrap();
    let merge0_github::webhook::WebhookEvent::PrMerged {
        pr_url, merge_sha, ..
    } = event
    else {
        panic!("expected merged event");
    };
    let id = tenant.report_for_pr(&pr_url).await.unwrap().unwrap();
    assert_eq!(id, report.id);
    tenant
        .record_merge_sha(id, merge_sha.as_deref().unwrap())
        .await
        .unwrap();
    tenant
        .record_outcome(
            id,
            OutcomeKind::Merged,
            Some(&pr_url),
            merged_at,
            None,
            Some(95_000),
        )
        .await
        .unwrap();
    tenant
        .set_report_status(id, ReportStatus::Completed)
        .await
        .unwrap();

    // ---- Telemetry (P0-10) ----
    let snapshot = tenant
        .telemetry(30, merged_at + Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(snapshot.counts.prs_merged, 1);
    assert_eq!(snapshot.merge_rate, Some(1.0));
    assert_eq!(snapshot.tokens_per_merged_pr, Some(95_000.0));

    // ---- Hardening pass (§5c): fix merged → prevention PR ----
    let candidates = merge0_hardening::find_candidates(&tenant).await.unwrap();
    assert!(
        !candidates.is_empty(),
        "merged fix produces a hardening candidate"
    );
    let candidate = &candidates[0];
    let signal = tenant
        .signal_by_fingerprint(&candidate.fingerprint)
        .await
        .unwrap()
        .unwrap();
    let mechanism = merge0_hardening::synthesize(candidate, &signal);
    // TypeError defect class → the most deterministic mechanism: a lint rule.
    assert!(
        matches!(mechanism, merge0_hardening::Mechanism::LintRule { .. }),
        "hierarchy prefers lint for TypeError, got {mechanism:?}"
    );
    let proposal = merge0_hardening::propose(
        candidate,
        &mechanism,
        github.as_ref(),
        &repo,
        &tenant,
        Some(merge0_context::intent::MERGE0_TEMPLATE),
        merged_at + Duration::hours(2),
    )
    .await
    .unwrap();
    {
        let state = github.state.lock().unwrap();
        let (_, head, _, title, body) = state.created_prs.last().unwrap();
        assert!(title.starts_with("[hardening]"));
        assert!(head.starts_with("merge0/hardening-"));
        assert!(
            body.contains(&candidate.fingerprint),
            "evidence-linked PR body"
        );
    }
    // The hardening report reached the inbox; the pass never duplicates.
    let hardening_reports = tenant
        .list_reports(Some(ReportStatus::AwaitingReview))
        .await
        .unwrap();
    assert!(hardening_reports
        .iter()
        .any(|r| r.kind == ReportKind::Hardening));
    // The cross-source report carries two fingerprints (one per source);
    // hardening is keyed per fingerprint, so propose for the remainder too,
    // after which the candidate queue must drain.
    for candidate in merge0_hardening::find_candidates(&tenant).await.unwrap() {
        let signal = tenant
            .signal_by_fingerprint(&candidate.fingerprint)
            .await
            .unwrap()
            .unwrap();
        let mechanism = merge0_hardening::synthesize(&candidate, &signal);
        merge0_hardening::propose(
            &candidate,
            &mechanism,
            github.as_ref(),
            &repo,
            &tenant,
            Some(merge0_context::intent::MERGE0_TEMPLATE),
            merged_at + Duration::hours(2),
        )
        .await
        .unwrap();
    }
    assert!(
        merge0_hardening::find_candidates(&tenant)
            .await
            .unwrap()
            .is_empty(),
        "no duplicate hardening proposals"
    );

    // ---- Effectiveness: recurrence after merge is a hard negative ----
    let hardening_merged_at = merged_at + Duration::hours(6);
    ingest(&tenant, hardening_merged_at + Duration::days(2)).await; // fingerprint recurs
    let effectiveness = merge0_hardening::effectiveness(
        &tenant,
        &candidate.fingerprint,
        hardening_merged_at,
        hardening_merged_at + Duration::days(3),
    )
    .await
    .unwrap();
    assert!(effectiveness.recurred);
    merge0_hardening::record_hard_negative(
        &tenant,
        proposal.report_id,
        hardening_merged_at + Duration::days(3),
    )
    .await
    .unwrap();
    let outcomes = tenant
        .outcomes_for_report(proposal.report_id)
        .await
        .unwrap();
    assert!(outcomes.iter().any(|o| o.outcome == OutcomeKind::Reverted
        && o.note.as_deref() == Some("hardening ineffective: fingerprint recurred")));

    // ---- Meta-loop (§5d): telemetry as signals + config-change PR ----
    let unhealthy = merge0_signal::telemetry::TelemetrySnapshot::from_counts(
        merge0_signal::telemetry::TelemetryCounts {
            window_days: 30,
            dispatched: 10,
            prs_opened: 6,
            prs_merged: 3,
            prs_closed: 3,
            prs_reverted: 1,
            reports_approved: 4,
            dismissals: [("intended_behavior".to_string(), 4u64)]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    // Telemetry ingested as just another Signal source.
    let meta_signals = merge0_meta::MetaAdapter
        .normalize(&serde_json::json!({
            "endpoint": "telemetry",
            "context": {"captured_at": "2026-08-07T06:00:00Z"},
            "payload": serde_json::to_value(&unhealthy).unwrap(),
        }))
        .unwrap();
    assert!(!meta_signals.is_empty());
    for signal in &meta_signals {
        assert_eq!(signal.source, merge0_signal::Source::Meta);
        tenant.upsert_signal(signal).await.unwrap();
    }
    // The meta-scout proposes evidence-linked config changes as ordinary PRs.
    let gate_toml = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/gate.toml"),
    )
    .unwrap();
    let proposals = merge0_meta::MetaScout::propose_config_changes(&unhealthy, &gate_toml);
    assert!(!proposals.is_empty());
    let config_proposal = &proposals[0];
    assert_eq!(config_proposal.path, "config/gate.toml");
    // The raised gate stays valid config and preserves the prompt.
    let raised: merge0_triage::config::GateConfig =
        toml::from_str(&config_proposal.new_content).unwrap();
    assert_eq!(raised.min_severity, Severity::High);
    assert!(config_proposal.new_content.contains("You are the gate"));
    merge0_meta::open_meta_pr(config_proposal, github.as_ref(), &repo)
        .await
        .unwrap();
    {
        let state = github.state.lock().unwrap();
        let (_, head, _, title, _) = state.created_prs.last().unwrap();
        assert!(title.starts_with("[meta]"));
        assert!(head.starts_with("merge0/meta-"));
    }

    store.drop_tenant(&schema).await.unwrap();
}

#[tokio::test]
async fn p2_broker_flow_work_order_scoped_short_lived_credentials() {
    use merge0_broker::{credential_helper, Broker, BrokerError, FakeMinter};

    let order = merge0_signal::WorkOrder {
        report_id: Ulid::new(),
        repo: "chalk/chalk".into(),
        summary: "s".into(),
        evidence: vec![],
        repro: "r".into(),
        suspect_change: None,
        success_criteria: "c".into(),
        constraints: String::new(),
        prior_attempts: vec![],
        diff_budget: Default::default(),
    };

    let mut broker = Broker::new(FakeMinter);
    broker.add_runner_key("tenant-runner-key");
    broker.register_grant_for(&order);
    let t0 = now();

    // Wrong key, wrong repo, unknown work order: all denied distinctly.
    assert!(matches!(
        broker.request_credentials(
            "wrong",
            &order.report_id.to_string(),
            "chalk/chalk",
            Duration::hours(1),
            t0
        ),
        Err(BrokerError::InvalidRunnerKey)
    ));
    assert!(matches!(
        broker.request_credentials(
            "tenant-runner-key",
            &order.report_id.to_string(),
            "chalk/other",
            Duration::hours(1),
            t0
        ),
        Err(BrokerError::RepoMismatch { .. })
    ));
    assert!(matches!(
        broker.request_credentials(
            "tenant-runner-key",
            "01UNKNOWN",
            "chalk/chalk",
            Duration::hours(1),
            t0
        ),
        Err(BrokerError::NoGrant(_))
    ));

    // The legitimate request: ≤10-minute repo-scoped credential.
    let credential = broker
        .request_credentials(
            "tenant-runner-key",
            &order.report_id.to_string(),
            "chalk/chalk",
            Duration::hours(1), // asked for an hour…
            t0,
        )
        .unwrap();
    assert!(
        credential.expires_at <= t0 + Duration::minutes(10),
        "…got ≤10min"
    );

    // The token reaches git only through the credential helper; debug output
    // never leaks it.
    assert!(!format!("{credential:?}").contains("fake-token"));
    let request =
        credential_helper::parse_request("protocol=https\nhost=github.com\npath=chalk/chalk.git\n")
            .unwrap();
    assert_eq!(request.host, "github.com");
    let response = credential_helper::format_response("x-access-token", &credential.token);
    assert!(response.contains("username=x-access-token"));
    assert!(response.contains("password=fake-token-chalk-chalk"));

    // Single-use: the dispatch's grant is consumed.
    assert!(matches!(
        broker.request_credentials(
            "tenant-runner-key",
            &order.report_id.to_string(),
            "chalk/chalk",
            Duration::minutes(5),
            t0
        ),
        Err(BrokerError::GrantConsumed(_))
    ));
}

#[tokio::test]
async fn p2_registry_flow_signed_index_to_manifest_change_pr() {
    use merge0_registry::{
        content_hash, install_plan, sign_index, verify_index, AcceptanceTelemetry, RegistryIndex,
        SkillListing, SkillPackage,
    };

    let files = vec![(
        "SKILL.md".to_string(),
        "# House style\nPrefer explicit errors over unwrap.".to_string(),
    )];
    let listing = SkillListing {
        name: "house-style".into(),
        version: "1.0.0".into(),
        description: "Chalk house style".into(),
        content_sha256: content_hash(&files),
        acceptance: Some(AcceptanceTelemetry {
            runs: 40,
            merge_rate: 0.8,
        }),
    };
    let index = RegistryIndex {
        generated_at: now(),
        listings: vec![listing.clone()],
    };

    // Sign with Merge0's registry key; verify against the pinned public key;
    // reject tampering.
    let signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let signed = sign_index(&index, &signing).unwrap();
    let verified = verify_index(&signed, &signing.verifying_key()).unwrap();
    assert_eq!(verified.listings[0].name, "house-style");
    let mut tampered = signed.clone();
    tampered.index_json = tampered.index_json.replace("house-style", "evil-style");
    assert!(verify_index(&tampered, &signing.verifying_key()).is_err());

    // Install = a manifest-change PR, never a server-side toggle.
    let package = SkillPackage { listing, files };
    let existing_manifest = r#"
[[mcp]]
name = "internal-api"
command = "npx our-api-mcp"
auth_env = "INTERNAL_API_KEY"

[network]
egress_allow = ["api.internal.example.com"]
"#;
    let plan = install_plan(&package, existing_manifest, "chalk/chalk", false).unwrap();
    assert_eq!(plan.branch_name, "merge0/skill-house-style-1.0.0");
    let manifest = &plan
        .files
        .iter()
        .find(|(path, _)| path == ".merge0/agent.toml")
        .unwrap()
        .1;
    // Customer-authored manifest content survives; skills table added.
    assert!(manifest.contains("internal-api"));
    assert!(manifest.contains("INTERNAL_API_KEY"));
    assert!(manifest.contains("egress_allow"));
    assert!(manifest.contains(".merge0/skills/"));

    // The plan lands as an ordinary PR through the same GitHub surface.
    let github = FakeGitHub::new();
    let repo = RepoRef::parse("chalk/chalk").unwrap();
    github
        .create_branch_with_files(&repo, &plan.branch_name, &plan.files, &plan.pr_title)
        .await
        .unwrap();
    github
        .create_pull_request(
            &repo,
            &plan.branch_name,
            "main",
            &plan.pr_title,
            &plan.pr_body,
        )
        .await
        .unwrap();
    let state = github.state.lock().unwrap();
    assert_eq!(state.created_prs.len(), 1);
    assert!(
        state.created_prs[0].4.contains("40 runs"),
        "telemetry cited in PR body"
    );
}
