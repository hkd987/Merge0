//! RBAC (PRD split table: "Multi-tenant org management, SSO/SAML, RBAC,
//! audit log" is `/ee` + hosted).
//!
//! A deliberately small matrix: three roles, seven actions, one pure
//! [`allowed`] function. The matches are exhaustive with no wildcard arms so
//! adding a role or action forces every combination to be decided
//! explicitly — the matrix can never silently default.

use crate::{enum_parse, enum_str, EeError, Result, TenantManager};
use chrono::{DateTime, Utc};
use sqlx::Row;
use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Reviewer,
    Viewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    ManageTenant,
    ManageMembers,
    ApproveReport,
    DismissReport,
    ViewInbox,
    ViewTelemetry,
    ManageBilling,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Admin, Role::Reviewer, Role::Viewer];
}

impl Action {
    pub const ALL: [Action; 7] = [
        Action::ManageTenant,
        Action::ManageMembers,
        Action::ApproveReport,
        Action::DismissReport,
        Action::ViewInbox,
        Action::ViewTelemetry,
        Action::ManageBilling,
    ];
}

/// The permission matrix. Admin may do everything; Reviewer works the inbox;
/// Viewer only observes.
// Explicit two-arm bool matches are the point here: no wildcard, so a new
// Role/Action variant fails to compile until every cell is decided.
#[allow(clippy::match_like_matches_macro)]
pub fn allowed(role: Role, action: Action) -> bool {
    match role {
        Role::Admin => match action {
            Action::ManageTenant
            | Action::ManageMembers
            | Action::ApproveReport
            | Action::DismissReport
            | Action::ViewInbox
            | Action::ViewTelemetry
            | Action::ManageBilling => true,
        },
        Role::Reviewer => match action {
            Action::ApproveReport
            | Action::DismissReport
            | Action::ViewInbox
            | Action::ViewTelemetry => true,
            Action::ManageTenant | Action::ManageMembers | Action::ManageBilling => false,
        },
        Role::Viewer => match action {
            Action::ViewInbox | Action::ViewTelemetry => true,
            Action::ManageTenant
            | Action::ManageMembers
            | Action::ApproveReport
            | Action::DismissReport
            | Action::ManageBilling => false,
        },
    }
}

impl TenantManager {
    /// Add (or re-role) a member of a tenant. Audited as `member.added`.
    pub async fn add_member(
        &self,
        tenant_id: Ulid,
        email: &str,
        role: Role,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        // Surface a typed not-found instead of an FK violation — and refuse
        // mutation on a suspended tenant. Suspension means frozen: the org's
        // data plane is refused by `tenant_store`, and its membership must
        // not drift underneath it either (an operator resumes first, then
        // edits, and both actions land in the audit trail in that order).
        let tenant = self.get_tenant(tenant_id).await?;
        if tenant.suspended {
            return Err(EeError::TenantSuspended(tenant_id.to_string()));
        }
        let sql = format!(
            "INSERT INTO {t} (tenant_id, email, role) VALUES ($1,$2,$3)
             ON CONFLICT (tenant_id, email) DO UPDATE SET role = EXCLUDED.role",
            t = Self::table("members")
        );
        sqlx::query(&sql)
            .bind(tenant_id.to_string())
            .bind(email)
            .bind(enum_str(&role))
            .execute(self.pool())
            .await?;
        self.record(
            Some(&tenant_id.to_string()),
            actor,
            "member.added",
            Some(email),
            Some(serde_json::json!({ "role": enum_str(&role) })),
            now,
        )
        .await
    }

    pub async fn member_role(&self, tenant_id: Ulid, email: &str) -> Result<Option<Role>> {
        let sql = format!(
            "SELECT role FROM {t} WHERE tenant_id = $1 AND email = $2",
            t = Self::table("members")
        );
        let row = sqlx::query(&sql)
            .bind(tenant_id.to_string())
            .bind(email)
            .fetch_optional(self.pool())
            .await?;
        row.map(|r| enum_parse(r.get("role"))).transpose()
    }

    /// Authorization check: `Ok(())` iff `email` is a member whose role
    /// permits `action`; otherwise [`EeError::Forbidden`] (non-members are
    /// forbidden, never "not found" — membership is not disclosed).
    pub async fn require(&self, tenant_id: Ulid, email: &str, action: Action) -> Result<()> {
        match self.member_role(tenant_id, email).await? {
            Some(role) if allowed(role, action) => Ok(()),
            _ => Err(EeError::Forbidden {
                tenant_id: tenant_id.to_string(),
                email: email.to_string(),
                action: enum_str(&action),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_matrix_is_exactly_as_specified() {
        use Action::*;
        use Role::*;
        // The full 3×7 matrix, spelled out.
        let table: &[(Role, Action, bool)] = &[
            (Admin, ManageTenant, true),
            (Admin, ManageMembers, true),
            (Admin, ApproveReport, true),
            (Admin, DismissReport, true),
            (Admin, ViewInbox, true),
            (Admin, ViewTelemetry, true),
            (Admin, ManageBilling, true),
            (Reviewer, ManageTenant, false),
            (Reviewer, ManageMembers, false),
            (Reviewer, ApproveReport, true),
            (Reviewer, DismissReport, true),
            (Reviewer, ViewInbox, true),
            (Reviewer, ViewTelemetry, true),
            (Reviewer, ManageBilling, false),
            (Viewer, ManageTenant, false),
            (Viewer, ManageMembers, false),
            (Viewer, ApproveReport, false),
            (Viewer, DismissReport, false),
            (Viewer, ViewInbox, true),
            (Viewer, ViewTelemetry, true),
            (Viewer, ManageBilling, false),
        ];
        assert_eq!(
            table.len(),
            Role::ALL.len() * Action::ALL.len(),
            "table must cover the full matrix"
        );
        for &(role, action, expected) in table {
            assert_eq!(
                allowed(role, action),
                expected,
                "allowed({role:?}, {action:?})"
            );
        }
    }

    #[test]
    fn roles_and_actions_serialize_snake_case() {
        assert_eq!(crate::enum_str(&Role::Admin), "admin");
        assert_eq!(crate::enum_str(&Role::Reviewer), "reviewer");
        assert_eq!(crate::enum_str(&Action::ApproveReport), "approve_report");
        let role: Role = crate::enum_parse("viewer").unwrap();
        assert_eq!(role, Role::Viewer);
        assert!(crate::enum_parse::<Role>("owner").is_err());
    }
}
