//! Scout `query_template` execution (audit M7): the template is a
//! deterministic filter over Signals, not decoration. The grammar is the
//! SQL-ish subset the shipped scouts already use — conditions joined by
//! `AND`/`OR` (`AND` binds tighter), plus an optional `ORDER BY`:
//!
//! ```text
//! kind = 'exception' AND first_seen >= {{period_start}} ORDER BY affected_count DESC
//! join_keys.stack_hash IS NOT NULL OR join_keys.url_path IS NOT NULL
//! severity >= 'high' AND affected_count >= 10
//! ```
//!
//! An empty template (or `*`) matches everything. Parsing is strict: an
//! unrecognized field, operator, or value is a config error that fails the
//! triage run loudly — a scout must never silently widen or narrow its
//! selection because of a typo (the same fail-closed posture as the gate).
//!
//! `{{period_start}}` is the only supported timestamp value; it resolves to
//! the start of the scout's schedule window at evaluation time.

use chrono::{DateTime, Utc};
use merge0_signal::{Severity, Signal, SignalKind, Source};

/// A parsed template: OR-of-AND condition groups plus an optional ordering.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Disjunctive normal form by construction: the signal matches if ANY
    /// group's conditions ALL hold. Empty = match-all.
    or_groups: Vec<Vec<Condition>>,
    pub order_by: Option<OrderBy>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderBy {
    pub field: OrderField,
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderField {
    AffectedCount,
    FirstSeen,
    LastSeen,
    Severity,
}

#[derive(Debug, Clone, PartialEq)]
enum Condition {
    KindIs(SignalKind),
    SourceIs(Source),
    SeverityIs(Severity),
    MinSeverity(Severity),
    MinAffectedCount(u64),
    IsNotNull(NullableField),
    SincePeriodStart(TimeField),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NullableField {
    StackHash,
    UrlPath,
    Release,
    AffectedCount,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TimeField {
    FirstSeen,
    LastSeen,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Num(u64),
    Eq,
    Ge,
}

fn tokenize(template: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '\'' => {
                chars.next();
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => value.push(c),
                        None => return Err("unterminated quoted string".into()),
                    }
                }
                tokens.push(Token::Str(value));
            }
            '=' => {
                chars.next();
                tokens.push(Token::Eq);
            }
            '>' => {
                chars.next();
                if chars.next_if_eq(&'=').is_none() {
                    return Err("unsupported operator '>' (use '>=')".into());
                }
                tokens.push(Token::Ge);
            }
            c if c.is_ascii_digit() => {
                let mut number = String::new();
                while let Some(d) = chars.next_if(|c| c.is_ascii_digit()) {
                    number.push(d);
                }
                tokens.push(Token::Num(
                    number.parse().map_err(|e| format!("bad number: {e}"))?,
                ));
            }
            c if c.is_alphanumeric() || c == '_' || c == '{' => {
                let mut ident = String::new();
                while let Some(d) = chars.next_if(|&c| c.is_alphanumeric() || "._{}".contains(c)) {
                    ident.push(d);
                }
                tokens.push(Token::Ident(ident));
            }
            other => return Err(format!("unexpected character {other:?}")),
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    /// Consume the next token if it is the given case-insensitive keyword.
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        if let Some(Token::Ident(word)) = self.peek() {
            if word.eq_ignore_ascii_case(keyword) {
                self.position += 1;
                return true;
            }
        }
        false
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<(), String> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(format!("expected {keyword} at token {:?}", self.peek()))
        }
    }

    fn parse_or(&mut self) -> Result<Vec<Vec<Condition>>, String> {
        let mut groups = vec![self.parse_and()?];
        while self.eat_keyword("OR") {
            groups.push(self.parse_and()?);
        }
        Ok(groups)
    }

    fn parse_and(&mut self) -> Result<Vec<Condition>, String> {
        let mut conditions = vec![self.parse_condition()?];
        while self.eat_keyword("AND") {
            conditions.push(self.parse_condition()?);
        }
        Ok(conditions)
    }

    fn parse_condition(&mut self) -> Result<Condition, String> {
        let field = match self.next() {
            Some(Token::Ident(name)) => name,
            other => return Err(format!("expected a field name, got {other:?}")),
        };
        // `<field> IS NOT NULL`
        if self.eat_keyword("IS") {
            self.expect_keyword("NOT")?;
            self.expect_keyword("NULL")?;
            let nullable = match field.as_str() {
                "join_keys.stack_hash" => NullableField::StackHash,
                "join_keys.url_path" => NullableField::UrlPath,
                "join_keys.release" => NullableField::Release,
                "affected_count" => NullableField::AffectedCount,
                other => return Err(format!("{other:?} does not support IS NOT NULL")),
            };
            return Ok(Condition::IsNotNull(nullable));
        }
        let operator = match self.next() {
            Some(token @ (Token::Eq | Token::Ge)) => token,
            other => {
                return Err(format!(
                    "expected '=' or '>=' after {field:?}, got {other:?}"
                ))
            }
        };
        let value = self.next();
        match (field.as_str(), operator, value) {
            ("kind", Token::Eq, Some(Token::Str(v))) => Ok(Condition::KindIs(parse_kind(&v)?)),
            ("source", Token::Eq, Some(Token::Str(v))) => {
                Ok(Condition::SourceIs(parse_source(&v)?))
            }
            ("severity", Token::Eq, Some(Token::Str(v))) => {
                Ok(Condition::SeverityIs(parse_severity(&v)?))
            }
            ("severity", Token::Ge, Some(Token::Str(v))) => {
                Ok(Condition::MinSeverity(parse_severity(&v)?))
            }
            ("affected_count", Token::Ge, Some(Token::Num(n))) => {
                Ok(Condition::MinAffectedCount(n))
            }
            ("first_seen", Token::Ge, Some(Token::Ident(v))) if v == "{{period_start}}" => {
                Ok(Condition::SincePeriodStart(TimeField::FirstSeen))
            }
            ("last_seen", Token::Ge, Some(Token::Ident(v))) if v == "{{period_start}}" => {
                Ok(Condition::SincePeriodStart(TimeField::LastSeen))
            }
            (field, operator, value) => Err(format!(
                "unsupported condition: {field} {operator:?} {value:?} \
                 (timestamps only compare >= {{{{period_start}}}})"
            )),
        }
    }

    fn parse_order_by(&mut self) -> Result<Option<OrderBy>, String> {
        if !self.eat_keyword("ORDER") {
            return Ok(None);
        }
        self.expect_keyword("BY")?;
        let field = match self.next() {
            Some(Token::Ident(name)) => match name.as_str() {
                "affected_count" => OrderField::AffectedCount,
                "first_seen" => OrderField::FirstSeen,
                "last_seen" => OrderField::LastSeen,
                "severity" => OrderField::Severity,
                other => return Err(format!("cannot ORDER BY {other:?}")),
            },
            other => return Err(format!("expected ORDER BY field, got {other:?}")),
        };
        let descending = if self.eat_keyword("DESC") {
            true
        } else {
            self.eat_keyword("ASC");
            false
        };
        Ok(Some(OrderBy { field, descending }))
    }
}

fn parse_kind(value: &str) -> Result<SignalKind, String> {
    match value {
        "exception" => Ok(SignalKind::Exception),
        "ux_friction" => Ok(SignalKind::UxFriction),
        "ticket" => Ok(SignalKind::Ticket),
        "regression" => Ok(SignalKind::Regression),
        "custom" => Ok(SignalKind::Custom),
        other => Err(format!("unknown kind {other:?}")),
    }
}

fn parse_severity(value: &str) -> Result<Severity, String> {
    match value {
        "low" => Ok(Severity::Low),
        "medium" => Ok(Severity::Medium),
        "high" => Ok(Severity::High),
        "critical" => Ok(Severity::Critical),
        other => Err(format!("unknown severity {other:?}")),
    }
}

fn parse_source(value: &str) -> Result<Source, String> {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .map_err(|_| format!("unknown source {value:?}"))
}

fn kind_matches(signal: &Signal, kind: SignalKind) -> bool {
    signal.kind == kind
}

impl Query {
    pub fn parse(template: &str) -> Result<Query, String> {
        let trimmed = template.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return Ok(Query {
                or_groups: Vec::new(),
                order_by: None,
            });
        }
        let mut parser = Parser {
            tokens: tokenize(trimmed)?,
            position: 0,
        };
        let or_groups = parser.parse_or()?;
        let order_by = parser.parse_order_by()?;
        if let Some(extra) = parser.peek() {
            return Err(format!("trailing tokens starting at {extra:?}"));
        }
        Ok(Query {
            or_groups,
            order_by,
        })
    }

    /// Does the signal satisfy this query? `period_start` resolves
    /// `{{period_start}}` (the start of the scout's schedule window).
    pub fn matches(&self, signal: &Signal, period_start: DateTime<Utc>) -> bool {
        if self.or_groups.is_empty() {
            return true;
        }
        self.or_groups.iter().any(|group| {
            group.iter().all(|condition| match condition {
                Condition::KindIs(kind) => kind_matches(signal, *kind),
                Condition::SourceIs(source) => signal.source == *source,
                Condition::SeverityIs(severity) => signal.severity == *severity,
                Condition::MinSeverity(severity) => signal.severity >= *severity,
                Condition::MinAffectedCount(n) => signal.affected_count.unwrap_or(0) >= *n,
                Condition::IsNotNull(field) => match field {
                    NullableField::StackHash => signal.join_keys.stack_hash.is_some(),
                    NullableField::UrlPath => signal.join_keys.url_path.is_some(),
                    NullableField::Release => signal.join_keys.release.is_some(),
                    NullableField::AffectedCount => signal.affected_count.is_some(),
                },
                Condition::SincePeriodStart(field) => match field {
                    TimeField::FirstSeen => signal.first_seen >= period_start,
                    TimeField::LastSeen => signal.last_seen >= period_start,
                },
            })
        })
    }

    /// Order a selection in place per the query's `ORDER BY` (no-op without
    /// one). Sorting is stable, so input order breaks ties.
    pub fn apply_order(&self, selection: &mut [&Signal]) {
        let Some(order) = self.order_by else {
            return;
        };
        let rank = |signal: &Signal| -> (u64, i64) {
            match order.field {
                OrderField::AffectedCount => (signal.affected_count.unwrap_or(0), 0),
                OrderField::FirstSeen => (0, signal.first_seen.timestamp_micros()),
                OrderField::LastSeen => (0, signal.last_seen.timestamp_micros()),
                OrderField::Severity => (signal.severity as u64, 0),
            }
        };
        if order.descending {
            selection.sort_by_key(|s| std::cmp::Reverse(rank(s)));
        } else {
            selection.sort_by_key(|s| rank(s));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use merge0_signal::JoinKeys;
    use ulid::Ulid;

    fn signal(kind: SignalKind, severity: Severity) -> Signal {
        let at = Utc.with_ymd_and_hms(2026, 8, 7, 0, 0, 0).unwrap();
        Signal {
            id: Ulid::new(),
            source: Source::Sentry,
            source_ref: "1".into(),
            kind,
            severity,
            title: "TypeError in districts".into(),
            body: String::new(),
            evidence: vec![],
            fingerprint: "f".into(),
            join_keys: JoinKeys::default(),
            affected_count: Some(12),
            first_seen: at,
            last_seen: at,
            raw: serde_json::Value::Null,
        }
    }

    fn period_start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 6, 0, 0, 0).unwrap()
    }

    #[test]
    fn empty_and_star_match_everything() {
        for template in ["", "   ", "*"] {
            let query = Query::parse(template).unwrap();
            assert!(query.matches(&signal(SignalKind::Ticket, Severity::Low), period_start()));
            assert!(query.order_by.is_none());
        }
    }

    #[test]
    fn kind_equality_filters() {
        let query = Query::parse("kind = 'exception'").unwrap();
        assert!(query.matches(
            &signal(SignalKind::Exception, Severity::High),
            period_start()
        ));
        assert!(!query.matches(&signal(SignalKind::Ticket, Severity::High), period_start()));
    }

    #[test]
    fn severity_floor_and_count_floor() {
        let query = Query::parse("severity >= 'high' AND affected_count >= 10").unwrap();
        assert!(query.matches(
            &signal(SignalKind::Exception, Severity::Critical),
            period_start()
        ));
        assert!(!query.matches(
            &signal(SignalKind::Exception, Severity::Medium),
            period_start()
        ));
        let mut few = signal(SignalKind::Exception, Severity::High);
        few.affected_count = Some(3);
        assert!(!query.matches(&few, period_start()));
    }

    #[test]
    fn or_of_is_not_null_over_join_keys() {
        let query =
            Query::parse("join_keys.stack_hash IS NOT NULL OR join_keys.url_path IS NOT NULL")
                .unwrap();
        let bare = signal(SignalKind::Exception, Severity::High);
        assert!(!query.matches(&bare, period_start()));
        let mut with_path = bare.clone();
        with_path.join_keys.url_path = Some("/districts".into());
        assert!(query.matches(&with_path, period_start()));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // ticket AND severity>='critical' OR exception → an ordinary
        // exception matches; an ordinary ticket does not.
        let query =
            Query::parse("kind = 'ticket' AND severity >= 'critical' OR kind = 'exception'")
                .unwrap();
        assert!(query.matches(
            &signal(SignalKind::Exception, Severity::Low),
            period_start()
        ));
        assert!(!query.matches(&signal(SignalKind::Ticket, Severity::High), period_start()));
        assert!(query.matches(
            &signal(SignalKind::Ticket, Severity::Critical),
            period_start()
        ));
    }

    #[test]
    fn period_start_resolves_against_the_window() {
        let query = Query::parse("first_seen >= {{period_start}}").unwrap();
        let fresh = signal(SignalKind::Exception, Severity::High);
        assert!(query.matches(&fresh, period_start()));
        let mut stale = fresh.clone();
        stale.first_seen = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        assert!(!query.matches(&stale, period_start()));
    }

    #[test]
    fn order_by_affected_count_desc() {
        let query = Query::parse("kind = 'exception' ORDER BY affected_count DESC").unwrap();
        let mut small = signal(SignalKind::Exception, Severity::High);
        small.affected_count = Some(1);
        let mut big = signal(SignalKind::Exception, Severity::High);
        big.affected_count = Some(100);
        let mut selection = vec![&small, &big];
        query.apply_order(&mut selection);
        assert_eq!(selection[0].affected_count, Some(100));
    }

    #[test]
    fn typos_fail_closed_not_silently() {
        for bad in [
            "kind = 'exceptions'",           // unknown kind value
            "kindd = 'exception'",           // unknown field
            "severity > 'high'",             // unsupported operator
            "first_seen >= '2026-01-01'",    // literal timestamps unsupported
            "kind = 'exception' ORDER BY x", // unknown order field
            "kind = 'exception' garbage",    // trailing tokens
            "title IS NOT NULL",             // non-nullable field
        ] {
            assert!(Query::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn the_shipped_scout_templates_parse() {
        for template in [
            "join_keys.stack_hash IS NOT NULL OR join_keys.url_path IS NOT NULL",
            "kind = 'exception' AND first_seen >= {{period_start}} ORDER BY affected_count DESC",
            "kind = 'exception' AND join_keys.release IS NOT NULL AND first_seen >= {{period_start}}",
        ] {
            Query::parse(template).unwrap_or_else(|e| panic!("{template:?}: {e}"));
        }
    }
}
