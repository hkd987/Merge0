//! Onboarding safety verification (PRD P0-9): Merge0 refuses to dispatch
//! until branch protection and required CI are confirmed on the default
//! branch. These are customer-side settings; Merge0 verifies, it does not
//! configure.

use crate::api::{GitHubApi, GitHubError, RepoRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyReport {
    pub default_branch: String,
    pub branch_protected: bool,
    pub required_checks: bool,
}

impl SafetyReport {
    /// Dispatch is allowed only when everything holds.
    pub fn satisfied(&self) -> bool {
        self.branch_protected && self.required_checks
    }

    pub fn failures(&self) -> Vec<&'static str> {
        let mut failures = Vec::new();
        if !self.branch_protected {
            failures.push("default branch has no branch protection");
        }
        if !self.required_checks {
            failures.push("no required status checks configured");
        }
        failures
    }
}

pub async fn verify_repo_safety(
    api: &dyn GitHubApi,
    repo: &RepoRef,
) -> Result<SafetyReport, GitHubError> {
    let default_branch = api.default_branch(repo).await?;
    let protection = api.branch_protection(repo, &default_branch).await?;
    Ok(SafetyReport {
        default_branch,
        branch_protected: protection.protected,
        required_checks: protection.required_checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{BranchProtection, FakeGitHub};

    #[tokio::test]
    async fn unprotected_repo_fails_with_reasons() {
        let api = FakeGitHub::new();
        let repo = RepoRef::parse("chalk/chalk").unwrap();
        let report = verify_repo_safety(&api, &repo).await.unwrap();
        assert!(!report.satisfied());
        assert_eq!(report.failures().len(), 2);
    }

    #[tokio::test]
    async fn fully_protected_repo_passes() {
        let api = FakeGitHub::new().with_protection(BranchProtection {
            protected: true,
            required_checks: true,
        });
        let repo = RepoRef::parse("chalk/chalk").unwrap();
        let report = verify_repo_safety(&api, &repo).await.unwrap();
        assert!(report.satisfied());
        assert!(report.failures().is_empty());
    }

    #[tokio::test]
    async fn protection_without_required_checks_still_fails() {
        let api = FakeGitHub::new().with_protection(BranchProtection {
            protected: true,
            required_checks: false,
        });
        let repo = RepoRef::parse("chalk/chalk").unwrap();
        let report = verify_repo_safety(&api, &repo).await.unwrap();
        assert!(!report.satisfied());
        assert_eq!(
            report.failures(),
            vec!["no required status checks configured"]
        );
    }
}
