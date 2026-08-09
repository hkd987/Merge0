//! CODEOWNERS-aware routing: parse the product repo's CODEOWNERS file and
//! map the file paths a report's evidence mentions to the teams/users who
//! own them, so a report lands in front of the right humans without anyone
//! reading a stack trace first.
//!
//! Semantics follow GitHub's documented CODEOWNERS rules: one pattern per
//! line followed by owners, `#` comments, **last matching pattern wins**,
//! gitignore-style globs except that negation (`!`) and character ranges
//! are not supported by GitHub — lines using them are ignored here exactly
//! as GitHub ignores them.

/// The three locations GitHub checks, in its own precedence order.
pub const CODEOWNERS_PATHS: &[&str] = &[".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    pattern: String,
    owners: Vec<String>,
}

/// A parsed CODEOWNERS file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeOwners {
    rules: Vec<Rule>,
}

impl CodeOwners {
    pub fn parse(content: &str) -> CodeOwners {
        let mut rules = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // GitHub ignores lines with negation or character ranges.
            if line.starts_with('!') || line.contains('[') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pattern) = parts.next() else {
                continue;
            };
            let owners: Vec<String> = parts
                .take_while(|p| !p.starts_with('#'))
                .map(str::to_string)
                .collect();
            // A pattern with no owners is valid in GitHub (it un-owns the
            // paths); keep it so last-match-wins can clear owners.
            rules.push(Rule {
                pattern: pattern.to_string(),
                owners,
            });
        }
        CodeOwners { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Owners for a repo-relative path — the LAST matching rule wins,
    /// exactly like GitHub.
    pub fn owners_for(&self, path: &str) -> Vec<String> {
        let path = path.trim_start_matches('/');
        self.rules
            .iter()
            .rev()
            .find(|rule| pattern_matches(&rule.pattern, path))
            .map(|rule| rule.owners.clone())
            .unwrap_or_default()
    }
}

/// Does a CODEOWNERS pattern match a repo-relative path?
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let anchored = pattern.contains('/')
        && !pattern
            .trim_end_matches('/')
            .trim_start_matches('/')
            .is_empty()
        && pattern.trim_end_matches('/').contains('/')
        || pattern.starts_with('/');
    let trimmed = pattern.trim_start_matches('/');
    let (core, dir_only) = match trimmed.strip_suffix('/') {
        Some(core) => (core, true),
        None => (trimmed, false),
    };
    if core.is_empty() {
        return false;
    }
    let regex = {
        // Glob → regex: `**` crosses directories, `*` does not, `?` is one
        // non-slash char. Everything else is literal.
        let mut out = String::from("^");
        if !anchored {
            out.push_str("(?:.*/)?");
        }
        let mut chars = core.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        // `**/` and bare `**`
                        if chars.peek() == Some(&'/') {
                            chars.next();
                            out.push_str("(?:.*/)?");
                        } else {
                            out.push_str(".*");
                        }
                    } else {
                        out.push_str("[^/]*");
                    }
                }
                '?' => out.push_str("[^/]"),
                other => {
                    if regex_syntax_char(other) {
                        out.push('\\');
                    }
                    out.push(other);
                }
            }
        }
        // A directory pattern owns everything beneath it; a pattern naming
        // a concrete file/dir may also own a directory's subtree (GitHub:
        // `docs` matches any file or directory named docs). A pattern
        // ENDING in a glob does not get the implicit subtree — GitHub
        // documents `docs/*` as matching files directly within docs only.
        if dir_only {
            out.push_str("/.*");
        } else if !core.ends_with('*') && !core.ends_with('?') {
            out.push_str("(?:/.*)?");
        }
        out.push('$');
        out
    };
    regex_lite_match(&regex, path)
}

fn regex_syntax_char(c: char) -> bool {
    matches!(
        c,
        '.' | '+' | '(' | ')' | '{' | '}' | '^' | '$' | '|' | '\\'
    )
}

/// Tiny anchored regex matcher for the limited syntax emitted above
/// (literals, `[^/]`, `[^/]*`, `.*`, `(?:.*/)?`, `(?:/.*)?`). Implemented
/// by hand so the crate takes no regex dependency for this.
fn regex_lite_match(regex: &str, path: &str) -> bool {
    // Compile to simple tokens.
    #[derive(Debug, Clone, PartialEq)]
    enum Tok {
        Lit(char),
        NotSlash,     // [^/]
        NotSlashStar, // [^/]*
        AnyStar,      // .*
        OptDirPrefix, // (?:.*/)?
        OptSubtree,   // (?:/.*)?
    }
    let mut toks = Vec::new();
    let mut rest = regex.strip_prefix('^').unwrap_or(regex);
    rest = rest.strip_suffix('$').unwrap_or(rest);
    let chars = rest.chars().collect::<Vec<_>>();
    let mut i = 0;
    while i < chars.len() {
        let remaining: String = chars[i..].iter().collect();
        if remaining.starts_with("(?:.*/)?") {
            toks.push(Tok::OptDirPrefix);
            i += 8;
        } else if remaining.starts_with("(?:/.*)?") {
            toks.push(Tok::OptSubtree);
            i += 8;
        } else if remaining.starts_with("[^/]*") {
            toks.push(Tok::NotSlashStar);
            i += 5;
        } else if remaining.starts_with("[^/]") {
            toks.push(Tok::NotSlash);
            i += 4;
        } else if remaining.starts_with(".*") {
            toks.push(Tok::AnyStar);
            i += 2;
        } else if chars[i] == '\\' && i + 1 < chars.len() {
            toks.push(Tok::Lit(chars[i + 1]));
            i += 2;
        } else {
            toks.push(Tok::Lit(chars[i]));
            i += 1;
        }
    }
    // Backtracking match.
    fn matches(toks: &[Tok], input: &[char]) -> bool {
        let Some(tok) = toks.first() else {
            return input.is_empty();
        };
        match tok {
            Tok::Lit(c) => input.first() == Some(c) && matches(&toks[1..], &input[1..]),
            Tok::NotSlash => {
                matches!(input.first(), Some(c) if *c != '/') && matches(&toks[1..], &input[1..])
            }
            Tok::NotSlashStar => {
                let mut end = 0;
                while end < input.len() && input[end] != '/' {
                    end += 1;
                }
                (0..=end)
                    .rev()
                    .any(|take| matches(&toks[1..], &input[take..]))
            }
            Tok::AnyStar => (0..=input.len())
                .rev()
                .any(|take| matches(&toks[1..], &input[take..])),
            Tok::OptDirPrefix => {
                // Either nothing, or any prefix ending in '/'.
                if matches(&toks[1..], input) {
                    return true;
                }
                (0..input.len())
                    .filter(|&i| input[i] == '/')
                    .any(|i| matches(&toks[1..], &input[i + 1..]))
            }
            Tok::OptSubtree => {
                if matches(&toks[1..], input) {
                    return true;
                }
                input.first() == Some(&'/') && toks.len() == 1
            }
        }
    }
    matches(&toks, &path.chars().collect::<Vec<_>>())
}

/// Pull repo-relative file paths out of free text (stack traces, report
/// summaries, work orders). Conservative: slash-separated tokens with a
/// file extension, not part of a URL, no traversal.
pub fn extract_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '(' | ')' | '"' | '\'' | ',' | ';' | '`' | '<' | '>' | '[' | ']'
            )
    }) {
        let token = raw
            .trim_start_matches('/')
            .trim_end_matches([':', '.', '!', '?']);
        // strip trailing :line:col
        let token = token.split(':').next().unwrap_or(token);
        if !token.contains('/') || token.contains("//") || token.contains("..") {
            continue;
        }
        if raw.contains("://") || token.starts_with("http") {
            continue;
        }
        let Some((_, file)) = token.rsplit_once('/') else {
            continue;
        };
        let Some((stem, ext)) = file.rsplit_once('.') else {
            continue;
        };
        let plausible_ext =
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric());
        let plausible_path = !stem.is_empty()
            && token.len() < 200
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));
        if plausible_ext && plausible_path && !out.contains(&token.to_string()) {
            out.push(token.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example block from GitHub's own CODEOWNERS documentation.
    const GITHUB_DOC_EXAMPLE: &str = r#"
# This is a comment.
*       @global-owner1 @global-owner2
*.js    @js-owner
*.go    docs@example.com
/build/logs/ @doctocat
docs/*  docs@example.com
apps/   @octocat
/docs/  @doctocat
"#;

    #[test]
    fn last_matching_pattern_wins_like_github() {
        let owners = CodeOwners::parse(GITHUB_DOC_EXAMPLE);
        // *.js beats * for JS files anywhere.
        assert_eq!(owners.owners_for("src/widget.js"), vec!["@js-owner"]);
        // /build/logs/ anchored directory.
        assert_eq!(owners.owners_for("build/logs/out.txt"), vec!["@doctocat"]);
        // docs/* is one level deep only — nested files fall back to *.
        assert_eq!(
            owners.owners_for("docs/getting-started.md"),
            vec!["@doctocat"] // /docs/ is later than docs/* and also matches
        );
        // apps/ matches the directory anywhere.
        assert_eq!(owners.owners_for("packages/apps/main.rs"), vec!["@octocat"]);
        // Fallback: the global rule.
        assert_eq!(
            owners.owners_for("README.txt"),
            vec!["@global-owner1", "@global-owner2"]
        );
        // Email owners parse.
        assert_eq!(owners.owners_for("cmd/main.go"), vec!["docs@example.com"]);
    }

    #[test]
    fn docs_star_is_single_level_when_not_shadowed() {
        let owners = CodeOwners::parse("*     @fallback\ndocs/*  @docs-team\n");
        assert_eq!(owners.owners_for("docs/intro.md"), vec!["@docs-team"]);
        // One level only — nested path falls through to the fallback.
        assert_eq!(owners.owners_for("docs/build/app.md"), vec!["@fallback"]);
    }

    #[test]
    fn double_star_crosses_directories_and_unowned_lines_clear() {
        let owners = CodeOwners::parse(
            "src/**/handlers/*.rs @backend\n/vendored/\n# vendored has NO owner\n",
        );
        assert_eq!(
            owners.owners_for("src/a/b/handlers/auth.rs"),
            vec!["@backend"]
        );
        assert!(owners.owners_for("vendored/lib.rs").is_empty());
        assert!(owners.owners_for("unmatched.txt").is_empty());
    }

    #[test]
    fn negation_and_ranges_are_ignored_like_github() {
        let owners = CodeOwners::parse("!ignored/ @x\n*.[jt]s @y\n*.rs @rust\n");
        assert_eq!(owners.owners_for("main.rs"), vec!["@rust"]);
        assert!(owners.owners_for("app.js").is_empty());
    }

    #[test]
    fn extract_paths_finds_source_files_not_urls() {
        let text = r#"
            TypeError at src/districts/summary.ts:42:7 in summarize()
            previously seen in `app/models/user.py` and ui/src/pages/Inbox.tsx.
            evidence: https://sentry.example.com/organizations/x/issues/1/
            not/a/path/because//double and ../traversal/x.rs
        "#;
        let paths = extract_paths(text);
        assert_eq!(
            paths,
            vec![
                "src/districts/summary.ts",
                "app/models/user.py",
                "ui/src/pages/Inbox.tsx",
            ]
        );
    }

    #[test]
    fn end_to_end_route_a_stack_trace_to_its_team() {
        let owners = CodeOwners::parse("ui/ @acme/frontend\nsrc/districts/ @acme/data\n");
        let trace = "crash in src/districts/summary.ts:42 (render at ui/src/App.tsx:9)";
        let mut routed: Vec<(String, Vec<String>)> = extract_paths(trace)
            .into_iter()
            .map(|p| {
                let o = owners.owners_for(&p);
                (p, o)
            })
            .filter(|(_, o)| !o.is_empty())
            .collect();
        routed.sort();
        assert_eq!(
            routed,
            vec![
                (
                    "src/districts/summary.ts".to_string(),
                    vec!["@acme/data".to_string()]
                ),
                (
                    "ui/src/App.tsx".to_string(),
                    vec!["@acme/frontend".to_string()]
                ),
            ]
        );
    }
}
