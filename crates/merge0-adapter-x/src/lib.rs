//! X (Twitter) → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `recent_search` — an X API v2 `GET /2/tweets/search/recent` response
//!   (`{"data": [...], "includes": {"users": [...]}, "meta": {...}}`), one
//!   `ticket` Signal per post. An absent or empty `data` array (X's shape
//!   for zero results) is zero Signals, not an error.
//!
//! Envelope context:
//! `{"query": "@acmeapp OR #acmeapp"}` — the operator-configured search the
//! fetch layer ran (mentions of the product handle and/or watched hashtags).
//! The query is recorded in each Signal body so triage can see *why* the
//! post was ingested.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Kind is `ticket`** by the same convention as the Slack and Reddit
//!   sources: a public post mentioning the product is unsolicited user
//!   feedback — the moral equivalent of a help-desk ticket filed in public.
//! - **Severity from engagement, conservative by design.** The engagement
//!   sum is `like + retweet + reply + quote`: ≥100 high, ≥25 medium, else
//!   low. A lone mention stays low — social chatter is noisy and a quiet
//!   inbox that's right beats a busy one. Medium (exactly the shipped
//!   gate's floor) requires 25+ visible reactions, i.e. the complaint
//!   resonates well beyond its author; 100+ is a viral moment for a
//!   product-sized audience and worth a human's attention. Never critical:
//!   engagement measures attention, not user impact — a joke can go viral.
//! - **`affected_count`** is that same engagement sum — each like, repost,
//!   reply, or quote is a distinct visible human reaction, the closest
//!   proxy X exposes for "how many people this resonates with" (an
//!   approximation: one account can both like and repost). `None` when 0 —
//!   no evidence anyone beyond the author is affected, and an absent count
//!   is more honest than an invented `1`.
//! - **`join_keys.account_id`** is the author's `@username` resolved from
//!   `includes.users` (the stable public handle a support team would join
//!   on, stored without the `@`), falling back to the numeric `author_id`
//!   when the expansion row is missing — v2 responses can omit it.
//! - **Title/body.** The title is the post text with whitespace runs
//!   collapsed, truncated to 80 *characters* (never bytes — posts are
//!   often non-ASCII) with a `…` marker. The body stays factual: the
//!   author, the engagement counts, the search query from context, and the
//!   full post text — never raw JSON.
//! - **Malformed rows** (missing `id`/`text`/`created_at`) are skipped,
//!   never an envelope failure: one corrupt row must not sink the page.
//! - **Evidence** is the id-only permalink
//!   `https://x.com/i/web/status/<id>`, which resolves without knowing the
//!   author's handle; kind `other` — social posts are neither tickets nor
//!   issues in the evidence taxonomy.
//! - **Fingerprint** hashes the post id: overlapping search windows
//!   re-yield the same posts across polls, and the id dedupes them.
//! - **`first_seen` = `last_seen` = `created_at`** — recent-search returns
//!   a point-in-time creation timestamp; X has no "last updated" for posts.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use ulid::Ulid;

pub struct XAdapter;

/// Typed context for the X envelope.
#[derive(Debug, Deserialize)]
struct Context {
    query: String,
}

/// The v2 recent-search response — only what we normalize; `meta` (paging)
/// is fetch-layer concern and each post is preserved verbatim via `raw`.
#[derive(Debug, Deserialize)]
struct SearchPage {
    /// Absent when `meta.result_count` is 0 — zero signals, not an error.
    #[serde(default)]
    data: Vec<serde_json::Value>,
    #[serde(default)]
    includes: Includes,
}

#[derive(Debug, Default, Deserialize)]
struct Includes {
    /// Kept untyped so one malformed expansion row cannot invalidate the
    /// page — each row is parsed (and skipped) individually.
    #[serde(default)]
    users: Vec<serde_json::Value>,
}

/// A user expansion row — only the fields needed to resolve a handle.
#[derive(Debug, Deserialize)]
struct User {
    id: String,
    username: String,
}

/// A post — the fields we normalize. Missing `id`/`text`/`created_at`
/// makes the row malformed (skipped, see module docs).
#[derive(Debug, Deserialize)]
struct Post {
    id: String,
    text: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    author_id: Option<String>,
    #[serde(default)]
    public_metrics: PublicMetrics,
}

#[derive(Debug, Default, Deserialize)]
struct PublicMetrics {
    #[serde(default)]
    retweet_count: u64,
    #[serde(default)]
    reply_count: u64,
    #[serde(default)]
    like_count: u64,
    #[serde(default)]
    quote_count: u64,
}

impl PublicMetrics {
    /// The engagement sum driving severity and `affected_count` (module
    /// docs).
    fn engagement(&self) -> u64 {
        self.like_count + self.retweet_count + self.reply_count + self.quote_count
    }
}

impl Adapter for XAdapter {
    fn source(&self) -> Source {
        Source::X
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid x context: {e}")))?;

        match envelope.endpoint.as_str() {
            "recent_search" => {
                let page: SearchPage =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!("invalid recent_search payload: {e}"))
                    })?;
                let usernames: BTreeMap<String, String> = page
                    .includes
                    .users
                    .iter()
                    .filter_map(|raw| serde_json::from_value::<User>(raw.clone()).ok())
                    .map(|user| (user.id, user.username))
                    .collect();
                Ok(page
                    .data
                    .iter()
                    .filter_map(|raw| normalize_post(raw, &usernames, &context.query))
                    .collect())
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one post; `None` means the row was malformed and is skipped
/// (module docs) — one corrupt row must not sink the page.
fn normalize_post(
    raw: &serde_json::Value,
    usernames: &BTreeMap<String, String>,
    query: &str,
) -> Option<Signal> {
    let post: Post = serde_json::from_value(raw.clone()).ok()?;

    let username = post
        .author_id
        .as_ref()
        .and_then(|id| usernames.get(id))
        .cloned();
    // The stable public handle when resolvable, the raw author id otherwise
    // (module docs).
    let account_id = username.clone().or_else(|| post.author_id.clone());
    let author_display = match (&username, &post.author_id) {
        (Some(handle), _) => format!("@{handle}"),
        (None, Some(id)) => format!("author id {id}"),
        (None, None) => "an unknown author".to_string(),
    };

    let metrics = &post.public_metrics;
    let engagement = metrics.engagement();
    let body = format!(
        "X post by {author_display} matching search query \"{query}\": \
         {} likes, {} reposts, {} replies, {} quotes.\n\n{}",
        metrics.like_count,
        metrics.retweet_count,
        metrics.reply_count,
        metrics.quote_count,
        post.text
    );

    Some(Signal {
        id: Ulid::generate(),
        source: Source::X,
        source_ref: post.id.clone(),
        kind: SignalKind::Ticket,
        severity: severity_from_engagement(engagement),
        title: title_from_text(&post.text),
        body,
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Other,
            label: match &username {
                Some(handle) => format!("X post by @{handle}"),
                None => "X post".to_string(),
            },
            // The id-only permalink form: resolves without the author's
            // handle (module docs).
            url: format!("https://x.com/i/web/status/{}", post.id),
        }],
        fingerprint: fingerprint(Source::X, &[&post.id]),
        join_keys: JoinKeys {
            account_id,
            ..Default::default()
        },
        affected_count: (engagement > 0).then_some(engagement),
        delegated: false,
        first_seen: post.created_at,
        last_seen: post.created_at,
        raw: raw.clone(),
    })
}

/// Engagement-based severity, conservative by design (see module docs).
fn severity_from_engagement(engagement: u64) -> Severity {
    match engagement {
        n if n >= 100 => Severity::High,
        n if n >= 25 => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Maximum title length in characters (not bytes — module docs).
const TITLE_MAX_CHARS: usize = 80;

/// Post text with whitespace runs collapsed, truncated to
/// [`TITLE_MAX_CHARS`] characters with a `…` marker. Character-based, never
/// a byte slice: post text is routinely non-ASCII and a byte cut would
/// panic mid-code-point.
fn title_from_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() <= TITLE_MAX_CHARS {
        collapsed
    } else {
        let mut title: String = chars[..TITLE_MAX_CHARS - 1].iter().collect();
        title.push('…');
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "query": "@acmeapp OR #acmeapp" },
            "payload": payload,
        })
    }

    fn minimal_post() -> serde_json::Value {
        serde_json::json!({
            "id": "1821091111222233344",
            "text": "The @acmeapp roster export drops the last student every time",
            "author_id": "4402987651",
            "created_at": "2026-08-08T09:14:03.000Z",
            "public_metrics": {
                "retweet_count": 0,
                "reply_count": 0,
                "like_count": 0,
                "quote_count": 0
            }
        })
    }

    fn includes() -> serde_json::Value {
        serde_json::json!({
            "users": [
                { "id": "4402987651", "name": "Yuki Tanaka", "username": "yuki_teaches" }
            ]
        })
    }

    fn normalize_one(post: serde_json::Value) -> Vec<Signal> {
        XAdapter
            .normalize(&envelope(
                "recent_search",
                serde_json::json!({ "data": [post], "includes": includes() }),
            ))
            .unwrap()
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(severity_from_engagement(0), Severity::Low);
        assert_eq!(severity_from_engagement(24), Severity::Low);
        assert_eq!(severity_from_engagement(25), Severity::Medium);
        assert_eq!(severity_from_engagement(99), Severity::Medium);
        assert_eq!(severity_from_engagement(100), Severity::High);
        assert_eq!(severity_from_engagement(10_000), Severity::High);
    }

    #[test]
    fn severity_sums_all_four_engagement_counts() {
        let mut post = minimal_post();
        post["public_metrics"] = serde_json::json!({
            "retweet_count": 7, "reply_count": 6, "like_count": 8, "quote_count": 4
        });
        let signal = &normalize_one(post)[0];
        // 7 + 6 + 8 + 4 = 25 → exactly the medium threshold.
        assert_eq!(signal.severity, Severity::Medium);
        assert_eq!(signal.affected_count, Some(25));
    }

    #[test]
    fn zero_engagement_means_no_affected_count() {
        let signal = &normalize_one(minimal_post())[0];
        assert_eq!(signal.affected_count, None);
        assert_eq!(signal.severity, Severity::Low);
    }

    #[test]
    fn title_truncates_at_char_boundaries_not_bytes() {
        let mut post = minimal_post();
        // 100 multi-byte characters: a byte slice at 80 would panic.
        let text: String = "同".repeat(100);
        post["text"] = serde_json::json!(text);
        let title = normalize_one(post)[0].title.clone();
        assert_eq!(title.chars().count(), 80);
        assert!(title.ends_with('…'));
        assert!(title.starts_with(&"同".repeat(79)));
    }

    #[test]
    fn short_text_is_the_whole_title_with_whitespace_collapsed() {
        let mut post = minimal_post();
        post["text"] = serde_json::json!("line one\nline  two");
        let signal = &normalize_one(post)[0];
        assert_eq!(signal.title, "line one line two");
        // The body keeps the original text verbatim.
        assert!(signal.body.contains("line one\nline  two"));
    }

    #[test]
    fn author_resolves_from_includes_into_body_and_join_keys() {
        let signal = &normalize_one(minimal_post())[0];
        assert_eq!(signal.join_keys.account_id.as_deref(), Some("yuki_teaches"));
        assert!(signal.body.starts_with("X post by @yuki_teaches"));
        assert!(signal.body.contains("\"@acmeapp OR #acmeapp\""));
        assert_eq!(signal.evidence[0].label, "X post by @yuki_teaches");
    }

    #[test]
    fn unresolved_author_falls_back_to_author_id() {
        let mut post = minimal_post();
        post["author_id"] = serde_json::json!("9999999999");
        let signal = &normalize_one(post)[0];
        assert_eq!(signal.join_keys.account_id.as_deref(), Some("9999999999"));
        assert!(signal.body.starts_with("X post by author id 9999999999"));
        assert_eq!(signal.evidence[0].label, "X post");
    }

    #[test]
    fn evidence_is_the_id_only_permalink() {
        let signal = &normalize_one(minimal_post())[0];
        assert_eq!(
            signal.evidence[0].url,
            "https://x.com/i/web/status/1821091111222233344"
        );
        assert_eq!(signal.evidence[0].kind, EvidenceKind::Other);
        assert_eq!(signal.source_ref, "1821091111222233344");
    }

    #[test]
    fn fingerprint_is_the_post_id_stable_across_engagement_changes() {
        let first = &normalize_one(minimal_post())[0];
        let mut repolled = minimal_post();
        repolled["public_metrics"]["like_count"] = serde_json::json!(50);
        assert_eq!(normalize_one(repolled)[0].fingerprint, first.fingerprint);
        assert_eq!(
            first.fingerprint,
            fingerprint(Source::X, &["1821091111222233344"])
        );

        let mut other = minimal_post();
        other["id"] = serde_json::json!("1821095555666677788");
        assert_ne!(normalize_one(other)[0].fingerprint, first.fingerprint);
    }

    #[test]
    fn timestamps_are_both_created_at() {
        let signal = &normalize_one(minimal_post())[0];
        assert_eq!(
            signal.first_seen,
            "2026-08-08T09:14:03Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(signal.first_seen, signal.last_seen);
    }

    #[test]
    fn absent_or_empty_data_is_zero_signals_not_an_error() {
        // X returns no `data` key at all when result_count is 0.
        let no_data = envelope(
            "recent_search",
            serde_json::json!({ "meta": { "result_count": 0 } }),
        );
        assert!(XAdapter.normalize(&no_data).unwrap().is_empty());

        let empty = envelope(
            "recent_search",
            serde_json::json!({ "data": [], "meta": { "result_count": 0 } }),
        );
        assert!(XAdapter.normalize(&empty).unwrap().is_empty());
    }

    #[test]
    fn malformed_rows_are_skipped_while_the_rest_normalize() {
        let mut no_text = minimal_post();
        no_text.as_object_mut().unwrap().remove("text");
        let mut bad_date = minimal_post();
        bad_date["created_at"] = serde_json::json!("not-a-date");
        let input = envelope(
            "recent_search",
            serde_json::json!({
                "data": [no_text, bad_date, minimal_post()],
                "includes": includes(),
            }),
        );
        let signals = XAdapter.normalize(&input).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_ref, "1821091111222233344");
    }

    #[test]
    fn missing_public_metrics_defaults_to_zero_engagement() {
        let mut post = minimal_post();
        post.as_object_mut().unwrap().remove("public_metrics");
        let signal = &normalize_one(post)[0];
        assert_eq!(signal.severity, Severity::Low);
        assert_eq!(signal.affected_count, None);
    }

    #[test]
    fn unknown_fields_are_ignored_but_preserved_in_raw() {
        let mut post = minimal_post();
        post["some_future_x_field"] = serde_json::json!({ "nested": true });
        let signal = &normalize_one(post)[0];
        assert_eq!(
            signal.raw["some_future_x_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn missing_context_query_is_malformed() {
        let input = serde_json::json!({
            "endpoint": "recent_search",
            "context": {},
            "payload": { "data": [] },
        });
        assert!(matches!(
            XAdapter.normalize(&input),
            Err(AdapterError::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("user_timeline", serde_json::json!({ "data": [] }));
        match XAdapter.normalize(&input) {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "user_timeline"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }
}
