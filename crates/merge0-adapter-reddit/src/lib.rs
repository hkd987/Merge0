//! Reddit → Signals.
//!
//! Supported envelope endpoints:
//!
//! - `subreddit_new` — a Reddit listing response for an operator-designated
//!   subreddit (`GET /r/{subreddit}/new.json`, shape `{"kind": "Listing",
//!   "data": {"children": [...]}}`), one `ticket` Signal per link post.
//!
//! Envelope context:
//! `{"base_url": "https://www.reddit.com"}` — the site base used to turn
//! each post's `permalink` into an evidence deep link.
//!
//! Normalization decisions (documented, not accidental):
//!
//! - **Kind is `ticket`** by convention: posts in a subreddit the operator
//!   deliberately watches (their own community, or one where their users
//!   congregate) are user feedback filed in public — the moral equivalent of
//!   a help-desk ticket, same as Slack watched-channel messages.
//! - **Only `t3` children normalize.** A listing page mixes kinds: `t3` is a
//!   link/self post, `t1` is a comment, and other kinds (ads, more-links)
//!   are pagination noise. Comments are follow-up discussion on a post we
//!   already ingest, not independent reports — non-`t3` children are
//!   skipped.
//! - **A malformed child** (missing `id`/`title`/`created_utc`, or a
//!   `created_utc` outside the representable timestamp range) is skipped,
//!   not an envelope failure: one corrupt row must never sink the rest of
//!   the page.
//! - **Title** is the post title verbatim — Reddit titles are already
//!   human-written summaries; prefixing or rewriting them would only bury
//!   the vendor's best one-liner.
//! - **`body`** stays factual — subreddit, author, score, comment count,
//!   then the `selftext` truncated at a character boundary to at most 500
//!   characters (UTF-8 safe, never a byte slice) with a `[…]` marker when
//!   cut; never raw JSON.
//! - **Severity** comes from engagement, `max(score, 0) + num_comments`:
//!   ≥100 high, ≥25 medium, else low. Conservative by design — votes and
//!   comments measure how many community members *care*, not a verified
//!   failure, so the scale never reaches critical (that is reserved for
//!   sources that observe breakage directly). 25+ combined engagement is a
//!   post the community is actively rallying around, which clears the gate
//!   floor so the gate decides; 100+ is unambiguous community-wide pain.
//! - **`affected_count`** is the post score when positive — upvotes are
//!   distinct-ish humans (one account, one vote; Reddit fuzzes the total
//!   but not the order of magnitude). A zero or negative score evidences no
//!   countable affected humans → `None`, never `Some(0)`.
//! - **`join_keys.account_id`** is the author username when present and
//!   meaningful. Reddit substitutes the literal `[deleted]` for removed
//!   accounts; that is a tombstone shared by unrelated posts, and joining
//!   on it would assert two strangers are the same account — it is treated
//!   as absent.
//! - **Evidence** is the canonical post permalink,
//!   `{base_url}{permalink}`, kind `other` — Reddit posts are neither
//!   tickets nor issues in the evidence taxonomy.
//! - **Fingerprint** hashes `[subreddit, post id]`: the id is
//!   vendor-stable, so every re-fetch of the same post (with drifting
//!   score/comment counts) dedupes to one Signal.
//! - **`first_seen`/`last_seen`** are both the post's `created_utc`
//!   (float epoch seconds; the fractional part is dropped) — a listing page
//!   carries no last-activity timestamp, and comment activity is not this
//!   Signal's lifecycle.

use chrono::{DateTime, Utc};
use merge0_adapters::{Adapter, AdapterError, Envelope};
use merge0_signal::{
    fingerprint, EvidenceKind, EvidenceLink, JoinKeys, Severity, Signal, SignalKind, Source,
};
use serde::Deserialize;
use ulid::Ulid;

pub struct RedditAdapter;

/// Longest `selftext` excerpt carried into `body` (characters, not bytes).
const SELFTEXT_MAX_CHARS: usize = 500;

/// Reddit's tombstone for a removed account — never a join key (module docs).
const DELETED_AUTHOR: &str = "[deleted]";

/// Typed context for the Reddit envelope.
#[derive(Debug, Deserialize)]
struct Context {
    base_url: String,
}

/// The outer listing: `{"kind": "Listing", "data": {"children": [...]}}`.
#[derive(Debug, Deserialize)]
struct Listing {
    data: ListingData,
}

#[derive(Debug, Deserialize)]
struct ListingData {
    children: Vec<serde_json::Value>,
}

/// A listing child's envelope: `{"kind": "t3", "data": {...}}`.
#[derive(Debug, Deserialize)]
struct Child {
    kind: String,
    data: serde_json::Value,
}

/// A `t3` post — only the fields we normalize; everything else is preserved
/// via `raw`. Missing `id`/`title`/`created_utc` fails this parse, which
/// skips the row (module docs).
#[derive(Debug, Deserialize)]
struct Post {
    id: String,
    title: String,
    created_utc: f64,
    #[serde(default)]
    selftext: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    subreddit: String,
    #[serde(default)]
    permalink: String,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    num_comments: u64,
}

impl Adapter for RedditAdapter {
    fn source(&self) -> Source {
        Source::Reddit
    }

    fn normalize(&self, input: &serde_json::Value) -> Result<Vec<Signal>, AdapterError> {
        let envelope: Envelope = serde_json::from_value(input.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid envelope: {e}")))?;
        let context: Context = serde_json::from_value(envelope.context.clone())
            .map_err(|e| AdapterError::Malformed(format!("invalid reddit context: {e}")))?;
        let base_url = context.base_url.trim_end_matches('/').to_string();

        match envelope.endpoint.as_str() {
            "subreddit_new" => {
                let listing: Listing =
                    serde_json::from_value(envelope.payload.clone()).map_err(|e| {
                        AdapterError::Malformed(format!(
                            "expected {{\"kind\": \"Listing\", \"data\": {{\"children\": \
                             [...]}}}}: {e}"
                        ))
                    })?;
                Ok(listing
                    .data
                    .children
                    .iter()
                    .filter_map(|child| normalize_child(child, &base_url))
                    .collect())
            }
            other => Err(AdapterError::UnsupportedEndpoint(other.to_string())),
        }
    }
}

/// Normalize one listing child; `None` means skipped — a non-`t3` kind or a
/// malformed row (module docs), never a silent drop of good data.
fn normalize_child(raw: &serde_json::Value, base_url: &str) -> Option<Signal> {
    let child: Child = serde_json::from_value(raw.clone()).ok()?;
    if child.kind != "t3" {
        return None;
    }
    let post: Post = serde_json::from_value(child.data).ok()?;
    let created = parse_created_utc(post.created_utc)?;

    let author = post
        .author
        .as_deref()
        .filter(|a| !a.is_empty() && *a != DELETED_AUTHOR);

    let mut body = format!(
        "Posted in r/{} by {}; score {}, {} comment{}.",
        post.subreddit,
        author.map_or_else(|| DELETED_AUTHOR.to_string(), |a| format!("u/{a}")),
        post.score,
        post.num_comments,
        if post.num_comments == 1 { "" } else { "s" },
    );
    if !post.selftext.is_empty() {
        body.push_str("\n\n");
        body.push_str(&truncate_chars(&post.selftext, SELFTEXT_MAX_CHARS));
    }

    let engagement = u64::try_from(post.score.max(0)).unwrap_or(0) + post.num_comments;

    Some(Signal {
        id: Ulid::new(),
        source: Source::Reddit,
        source_ref: post.id.clone(),
        kind: SignalKind::Ticket,
        severity: severity_from_engagement(engagement),
        title: post.title.clone(),
        body,
        evidence: vec![EvidenceLink {
            kind: EvidenceKind::Other,
            label: format!("r/{} post", post.subreddit),
            url: format!("{base_url}{}", post.permalink),
        }],
        fingerprint: fingerprint(Source::Reddit, &[&post.subreddit, &post.id]),
        join_keys: JoinKeys {
            account_id: author.map(str::to_string),
            ..Default::default()
        },
        affected_count: u64::try_from(post.score).ok().filter(|&s| s > 0),
        delegated: false,
        first_seen: created,
        last_seen: created,
        raw: raw.clone(),
    })
}

/// Convert Reddit's float epoch seconds; `None` (→ row skip) on NaN,
/// infinities, or values outside chrono's representable range — garbage
/// timestamps must never panic or masquerade as the epoch.
fn parse_created_utc(created_utc: f64) -> Option<DateTime<Utc>> {
    if !created_utc.is_finite() {
        return None;
    }
    // `as i64` saturates; chrono then rejects the saturated extremes.
    DateTime::from_timestamp(created_utc as i64, 0)
}

/// Engagement-based severity (see module docs for the reasoning).
fn severity_from_engagement(engagement: u64) -> Severity {
    match engagement {
        n if n >= 100 => Severity::High,
        n if n >= 25 => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Truncate to at most `max` characters (UTF-8 safe — never a byte slice),
/// appending a `[…]` marker when anything was cut.
fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((byte_at, _)) => format!("{} […]", &text[..byte_at]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(endpoint: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "endpoint": endpoint,
            "context": { "base_url": "https://www.reddit.com" },
            "payload": payload,
        })
    }

    fn listing(children: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "kind": "Listing",
            "data": { "after": "t3_1zzz99", "dist": children.len(), "children": children }
        })
    }

    fn post(id: &str, score: i64, num_comments: u64) -> serde_json::Value {
        serde_json::json!({
            "kind": "t3",
            "data": {
                "id": id,
                "name": format!("t3_{id}"),
                "title": "Roster CSV export downloads an empty file",
                "selftext": "Anyone else? Exporting a roster over 500 students gives a 0-byte file.",
                "author": "example_teacher_01",
                "subreddit": "chalkapp",
                "permalink": format!("/r/chalkapp/comments/{id}/roster_csv_export_empty/"),
                "url": format!("https://www.reddit.com/r/chalkapp/comments/{id}/roster_csv_export_empty/"),
                "score": score,
                "num_comments": num_comments,
                "upvote_ratio": 0.93,
                "created_utc": 1723100000.0
            }
        })
    }

    fn normalize(children: Vec<serde_json::Value>) -> Vec<Signal> {
        RedditAdapter
            .normalize(&envelope("subreddit_new", listing(children)))
            .unwrap()
    }

    #[test]
    fn severity_thresholds_from_combined_engagement() {
        assert_eq!(severity_from_engagement(0), Severity::Low);
        assert_eq!(severity_from_engagement(24), Severity::Low);
        assert_eq!(severity_from_engagement(25), Severity::Medium);
        assert_eq!(severity_from_engagement(99), Severity::Medium);
        assert_eq!(severity_from_engagement(100), Severity::High);
        // Score and comments sum: 20 + 5 clears the medium bar.
        assert_eq!(
            normalize(vec![post("1abc23", 20, 5)])[0].severity,
            Severity::Medium
        );
        // A negative score never subtracts from comment engagement.
        assert_eq!(
            normalize(vec![post("1abc23", -50, 30)])[0].severity,
            Severity::Medium
        );
    }

    #[test]
    fn affected_count_is_positive_score_or_none() {
        assert_eq!(
            normalize(vec![post("1abc23", 41, 2)])[0].affected_count,
            Some(41)
        );
        assert_eq!(
            normalize(vec![post("1abc23", 0, 2)])[0].affected_count,
            None
        );
        assert_eq!(
            normalize(vec![post("1abc23", -3, 2)])[0].affected_count,
            None
        );
    }

    #[test]
    fn non_t3_children_are_skipped() {
        let comment = serde_json::json!({
            "kind": "t1",
            "data": { "id": "k9xyz1", "body": "same here", "author": "example_parent_02",
                      "created_utc": 1723100500.0 }
        });
        let signals = normalize(vec![comment, post("1abc23", 5, 1)]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_ref, "1abc23");
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        let mut no_title = post("1bad01", 5, 1);
        no_title["data"].as_object_mut().unwrap().remove("title");
        let mut no_created = post("1bad02", 5, 1);
        no_created["data"]
            .as_object_mut()
            .unwrap()
            .remove("created_utc");
        let mut nan_created = post("1bad03", 5, 1);
        nan_created["data"]["created_utc"] = serde_json::json!("not-a-number");
        let signals = normalize(vec![
            no_title,
            no_created,
            nan_created,
            post("1abc23", 5, 1),
        ]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_ref, "1abc23");
    }

    #[test]
    fn out_of_range_created_utc_skips_without_panicking() {
        assert!(parse_created_utc(f64::NAN).is_none());
        assert!(parse_created_utc(f64::INFINITY).is_none());
        assert!(parse_created_utc(1e30).is_none());
        assert_eq!(
            parse_created_utc(1723100000.9),
            DateTime::from_timestamp(1_723_100_000, 0)
        );
        let mut huge = post("1bad04", 5, 1);
        huge["data"]["created_utc"] = serde_json::json!(1e30);
        assert!(normalize(vec![huge]).is_empty());
    }

    #[test]
    fn selftext_truncates_on_char_boundaries_never_bytes() {
        // Multi-byte chars right at the cut: a byte slice would panic here.
        let mut long = post("1abc23", 5, 1);
        long["data"]["selftext"] = serde_json::json!("é".repeat(600));
        let body = &normalize(vec![long])[0].body;
        assert!(body.ends_with(&format!("{} […]", "é".repeat(500))));

        assert_eq!(truncate_chars("short", 500), "short");
        let exactly = "x".repeat(500);
        assert_eq!(truncate_chars(&exactly, 500), exactly);
    }

    #[test]
    fn body_is_factual_and_title_is_verbatim() {
        let signal = &normalize(vec![post("1abc23", 41, 1)])[0];
        assert_eq!(signal.title, "Roster CSV export downloads an empty file");
        assert!(signal
            .body
            .starts_with("Posted in r/chalkapp by u/example_teacher_01; score 41, 1 comment."));
        assert!(signal.body.contains("0-byte file"));
        assert!(!signal.body.contains('{'), "body must never be raw JSON");
        assert_eq!(signal.kind, SignalKind::Ticket);
    }

    #[test]
    fn evidence_deep_links_via_context_base_url() {
        let signal = &normalize(vec![post("1abc23", 5, 1)])[0];
        assert_eq!(
            signal.evidence[0].url,
            "https://www.reddit.com/r/chalkapp/comments/1abc23/roster_csv_export_empty/"
        );
        assert_eq!(signal.evidence[0].label, "r/chalkapp post");
        assert_eq!(signal.evidence[0].kind, EvidenceKind::Other);

        // A trailing slash on base_url must not double up.
        let input = serde_json::json!({
            "endpoint": "subreddit_new",
            "context": { "base_url": "https://www.reddit.com/" },
            "payload": listing(vec![post("1abc23", 5, 1)]),
        });
        let signal = &RedditAdapter.normalize(&input).unwrap()[0];
        assert_eq!(
            signal.evidence[0].url,
            "https://www.reddit.com/r/chalkapp/comments/1abc23/roster_csv_export_empty/"
        );
    }

    #[test]
    fn fingerprint_is_stable_across_refetches_with_drifting_counts() {
        let first = &normalize(vec![post("1abc23", 5, 1)])[0];
        let refetched = &normalize(vec![post("1abc23", 90, 40)])[0];
        assert_eq!(first.fingerprint, refetched.fingerprint);
        assert_eq!(
            first.fingerprint,
            fingerprint(Source::Reddit, &["chalkapp", "1abc23"])
        );
        assert_ne!(
            first.fingerprint,
            normalize(vec![post("1zzz99", 5, 1)])[0].fingerprint
        );
    }

    #[test]
    fn author_becomes_account_id_but_deleted_does_not() {
        let signal = &normalize(vec![post("1abc23", 5, 1)])[0];
        assert_eq!(
            signal.join_keys.account_id.as_deref(),
            Some("example_teacher_01")
        );

        let mut deleted = post("1abc23", 5, 1);
        deleted["data"]["author"] = serde_json::json!("[deleted]");
        let signal = &normalize(vec![deleted])[0];
        assert!(signal.join_keys.account_id.is_none());

        let mut absent = post("1abc23", 5, 1);
        absent["data"].as_object_mut().unwrap().remove("author");
        assert!(normalize(vec![absent])[0].join_keys.account_id.is_none());
    }

    #[test]
    fn timestamps_are_created_utc_for_both_seen_fields() {
        let signal = &normalize(vec![post("1abc23", 5, 1)])[0];
        assert_eq!(
            signal.first_seen,
            DateTime::from_timestamp(1_723_100_000, 0).unwrap()
        );
        assert_eq!(signal.first_seen, signal.last_seen);
    }

    #[test]
    fn unsupported_endpoint_is_rejected() {
        let input = envelope("subreddit_hot", listing(vec![]));
        match RedditAdapter.normalize(&input) {
            Err(AdapterError::UnsupportedEndpoint(name)) => assert_eq!(name, "subreddit_hot"),
            other => panic!("expected UnsupportedEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn raw_preserves_the_whole_child_verbatim() {
        let mut extra = post("1abc23", 5, 1);
        extra["data"]["some_future_reddit_field"] = serde_json::json!({ "nested": true });
        let signals = normalize(vec![extra]);
        assert_eq!(signals[0].raw["kind"], "t3");
        assert_eq!(
            signals[0].raw["data"]["some_future_reddit_field"]["nested"],
            serde_json::Value::Bool(true)
        );
    }
}
