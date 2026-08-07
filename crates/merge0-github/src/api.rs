//! The GitHub API surface Merge0 uses — a trait, so every consumer
//! (runner, hardening, meta-loop, safety checks) is testable against
//! [`FakeGitHub`] and the real client stays in one place.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("GitHub API returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("invalid repo reference {0:?} (expected owner/name)")]
    BadRepoRef(String),
    #[error("auth error: {0}")]
    Auth(String),
}

/// `owner/name`, validated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn parse(full: &str) -> Result<Self, GitHubError> {
        let mut parts = full.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
                Ok(RepoRef {
                    owner: owner.to_string(),
                    name: name.to_string(),
                })
            }
            _ => Err(GitHubError::BadRepoRef(full.to_string())),
        }
    }

    pub fn full(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    pub head_branch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub tag: String,
    pub sha: Option<String>,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BranchProtection {
    pub protected: bool,
    pub required_checks: bool,
}

#[async_trait]
pub trait GitHubApi: Send + Sync {
    async fn repository_dispatch(
        &self,
        repo: &RepoRef,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(), GitHubError>;

    async fn default_branch(&self, repo: &RepoRef) -> Result<String, GitHubError>;

    async fn branch_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<BranchProtection, GitHubError>;

    /// Create a branch from the default branch head and commit files to it
    /// (used by hardening and meta-loop PRs; contents API, one commit).
    async fn create_branch_with_files(
        &self,
        repo: &RepoRef,
        branch: &str,
        files: &[(String, String)],
        message: &str,
    ) -> Result<(), GitHubError>;

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrInfo, GitHubError>;

    async fn list_releases(&self, repo: &RepoRef) -> Result<Vec<ReleaseInfo>, GitHubError>;
}

/// Trait objects behind `Arc` are first-class API handles (the server holds
/// one shared client across handlers).
#[async_trait]
impl<T: GitHubApi + ?Sized> GitHubApi for std::sync::Arc<T> {
    async fn repository_dispatch(
        &self,
        repo: &RepoRef,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(), GitHubError> {
        (**self)
            .repository_dispatch(repo, event_type, payload)
            .await
    }

    async fn default_branch(&self, repo: &RepoRef) -> Result<String, GitHubError> {
        (**self).default_branch(repo).await
    }

    async fn branch_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<BranchProtection, GitHubError> {
        (**self).branch_protection(repo, branch).await
    }

    async fn create_branch_with_files(
        &self,
        repo: &RepoRef,
        branch: &str,
        files: &[(String, String)],
        message: &str,
    ) -> Result<(), GitHubError> {
        (**self)
            .create_branch_with_files(repo, branch, files, message)
            .await
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrInfo, GitHubError> {
        (**self)
            .create_pull_request(repo, head, base, title, body)
            .await
    }

    async fn list_releases(&self, repo: &RepoRef) -> Result<Vec<ReleaseInfo>, GitHubError> {
        (**self).list_releases(repo).await
    }
}

// ---- Real client ----

/// REST client. Every call authenticates with a fresh short-lived
/// installation token from the [`crate::auth::AppAuth`] flow — tokens are
/// never cached beyond a call sequence and never logged (P0-11).
pub struct RestGitHub<T: crate::auth::TokenSource> {
    pub(crate) client: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) tokens: T,
}

impl<T: crate::auth::TokenSource> RestGitHub<T> {
    pub fn new(tokens: T) -> Self {
        RestGitHub {
            client: reqwest::Client::new(),
            base_url: "https://api.github.com".into(),
            tokens,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, GitHubError> {
        let token = self.tokens.token().await?;
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base_url))
            .header("authorization", format!("Bearer {token}"))
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "merge0");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| GitHubError::Transport(e.to_string()))?;
        let status = response.status();
        let value: serde_json::Value = if status == reqwest::StatusCode::NO_CONTENT {
            serde_json::Value::Null
        } else {
            response.json().await.unwrap_or(serde_json::Value::Null)
        };
        if !status.is_success() {
            return Err(GitHubError::Api {
                status: status.as_u16(),
                message: value["message"].as_str().unwrap_or("unknown").to_string(),
            });
        }
        Ok(value)
    }
}

#[async_trait]
impl<T: crate::auth::TokenSource> GitHubApi for RestGitHub<T> {
    async fn repository_dispatch(
        &self,
        repo: &RepoRef,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(), GitHubError> {
        self.request(
            reqwest::Method::POST,
            &format!("/repos/{}/{}/dispatches", repo.owner, repo.name),
            Some(&serde_json::json!({
                "event_type": event_type,
                "client_payload": payload,
            })),
        )
        .await?;
        Ok(())
    }

    async fn default_branch(&self, repo: &RepoRef) -> Result<String, GitHubError> {
        let value = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{}/{}", repo.owner, repo.name),
                None,
            )
            .await?;
        Ok(value["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string())
    }

    async fn branch_protection(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<BranchProtection, GitHubError> {
        match self
            .request(
                reqwest::Method::GET,
                &format!(
                    "/repos/{}/{}/branches/{branch}/protection",
                    repo.owner, repo.name
                ),
                None,
            )
            .await
        {
            Ok(value) => Ok(BranchProtection {
                protected: true,
                required_checks: value["required_status_checks"].is_object(),
            }),
            // 404 means the branch is unprotected, not an error.
            Err(GitHubError::Api { status: 404, .. }) => Ok(BranchProtection::default()),
            Err(e) => Err(e),
        }
    }

    async fn create_branch_with_files(
        &self,
        repo: &RepoRef,
        branch: &str,
        files: &[(String, String)],
        message: &str,
    ) -> Result<(), GitHubError> {
        let base = self.default_branch(repo).await?;
        let head = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{}/{}/git/ref/heads/{base}", repo.owner, repo.name),
                None,
            )
            .await?;
        let sha = head["object"]["sha"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        self.request(
            reqwest::Method::POST,
            &format!("/repos/{}/{}/git/refs", repo.owner, repo.name),
            Some(&serde_json::json!({
                "ref": format!("refs/heads/{branch}"),
                "sha": sha,
            })),
        )
        .await?;
        for (path, content) in files {
            use base64_mini::encode as b64;
            // Fetch existing file sha on the new branch (update vs create).
            let existing = self
                .request(
                    reqwest::Method::GET,
                    &format!(
                        "/repos/{}/{}/contents/{path}?ref={branch}",
                        repo.owner, repo.name
                    ),
                    None,
                )
                .await
                .ok();
            let mut body = serde_json::json!({
                "message": message,
                "content": b64(content.as_bytes()),
                "branch": branch,
            });
            if let Some(existing) = existing {
                if let Some(sha) = existing["sha"].as_str() {
                    body["sha"] = serde_json::Value::String(sha.to_string());
                }
            }
            self.request(
                reqwest::Method::PUT,
                &format!("/repos/{}/{}/contents/{path}", repo.owner, repo.name),
                Some(&body),
            )
            .await?;
        }
        Ok(())
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrInfo, GitHubError> {
        let value = self
            .request(
                reqwest::Method::POST,
                &format!("/repos/{}/{}/pulls", repo.owner, repo.name),
                Some(&serde_json::json!({
                    "title": title, "body": body, "head": head, "base": base,
                })),
            )
            .await?;
        Ok(PrInfo {
            number: value["number"].as_u64().unwrap_or_default(),
            url: value["html_url"].as_str().unwrap_or_default().to_string(),
            head_branch: head.to_string(),
        })
    }

    async fn list_releases(&self, repo: &RepoRef) -> Result<Vec<ReleaseInfo>, GitHubError> {
        let value = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{}/{}/releases?per_page=100", repo.owner, repo.name),
                None,
            )
            .await?;
        let releases = value
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                Some(ReleaseInfo {
                    tag: r["tag_name"].as_str()?.to_string(),
                    sha: None,
                    published_at: r["published_at"].as_str().and_then(|s| s.parse().ok())?,
                    notes: r["body"].as_str().map(String::from),
                })
            })
            .collect();
        Ok(releases)
    }
}

/// Minimal base64 (standard alphabet, padded) — avoids a dependency for the
/// single contents-API use.
mod base64_mini {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
            out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
            out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }
}

// ---- Fake ----

/// Recording fake for tests: configure state, assert on calls.
#[derive(Default)]
pub struct FakeGitHub {
    pub state: std::sync::Mutex<FakeState>,
}

/// A `create_branch_with_files` call as the fake recorded it.
pub type RecordedBranch = (RepoRef, String, Vec<(String, String)>, String);
/// A `create_pull_request` call: (repo, head, base, title, body).
pub type RecordedPr = (RepoRef, String, String, String, String);

#[derive(Default)]
pub struct FakeState {
    pub dispatches: Vec<(RepoRef, String, serde_json::Value)>,
    pub created_branches: Vec<RecordedBranch>,
    pub created_prs: Vec<RecordedPr>,
    pub protection: BranchProtection,
    pub releases: Vec<ReleaseInfo>,
    pub default_branch: String,
    pub next_pr_number: u64,
}

impl FakeGitHub {
    pub fn new() -> Self {
        let fake = FakeGitHub::default();
        {
            let mut state = fake.state.lock().unwrap();
            state.default_branch = "main".into();
            state.next_pr_number = 1;
        }
        fake
    }

    pub fn with_protection(self, protection: BranchProtection) -> Self {
        self.state.lock().unwrap().protection = protection;
        self
    }
}

#[async_trait]
impl GitHubApi for FakeGitHub {
    async fn repository_dispatch(
        &self,
        repo: &RepoRef,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<(), GitHubError> {
        self.state.lock().unwrap().dispatches.push((
            repo.clone(),
            event_type.to_string(),
            payload.clone(),
        ));
        Ok(())
    }

    async fn default_branch(&self, _repo: &RepoRef) -> Result<String, GitHubError> {
        Ok(self.state.lock().unwrap().default_branch.clone())
    }

    async fn branch_protection(
        &self,
        _repo: &RepoRef,
        _branch: &str,
    ) -> Result<BranchProtection, GitHubError> {
        Ok(self.state.lock().unwrap().protection.clone())
    }

    async fn create_branch_with_files(
        &self,
        repo: &RepoRef,
        branch: &str,
        files: &[(String, String)],
        message: &str,
    ) -> Result<(), GitHubError> {
        self.state.lock().unwrap().created_branches.push((
            repo.clone(),
            branch.to_string(),
            files.to_vec(),
            message.to_string(),
        ));
        Ok(())
    }

    async fn create_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrInfo, GitHubError> {
        let mut state = self.state.lock().unwrap();
        let number = state.next_pr_number;
        state.next_pr_number += 1;
        state.created_prs.push((
            repo.clone(),
            head.to_string(),
            base.to_string(),
            title.to_string(),
            body.to_string(),
        ));
        Ok(PrInfo {
            number,
            url: format!("https://github.com/{}/pull/{number}", repo.full()),
            head_branch: head.to_string(),
        })
    }

    async fn list_releases(&self, _repo: &RepoRef) -> Result<Vec<ReleaseInfo>, GitHubError> {
        Ok(self.state.lock().unwrap().releases.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_parses_and_rejects() {
        let repo = RepoRef::parse("chalk/chalk-app").unwrap();
        assert_eq!(repo.full(), "chalk/chalk-app");
        for bad in ["chalk", "a/b/c", "/x", "x/", ""] {
            assert!(RepoRef::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(super::base64_mini::encode(b""), "");
        assert_eq!(super::base64_mini::encode(b"f"), "Zg==");
        assert_eq!(super::base64_mini::encode(b"fo"), "Zm8=");
        assert_eq!(super::base64_mini::encode(b"foo"), "Zm9v");
        assert_eq!(super::base64_mini::encode(b"foobar"), "Zm9vYmFy");
    }
}
