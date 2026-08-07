//! GitHub App authentication (PRD §5a, P0-11).
//!
//! The only credential class in the system: a short-lived App JWT exchanged
//! for a repo-scoped installation token (~1h GitHub-side lifetime, minted
//! per request, never persisted). No PATs, no user OAuth tokens.

use crate::api::GitHubError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
pub struct AppJwtClaims {
    /// Issued-at, backdated 60s for clock drift (GitHub recommendation).
    pub iat: i64,
    /// Expiry — capped at 10 minutes, the GitHub maximum.
    pub exp: i64,
    /// The App ID.
    pub iss: String,
}

impl AppJwtClaims {
    pub fn new(app_id: &str, now: DateTime<Utc>) -> Self {
        AppJwtClaims {
            iat: now.timestamp() - 60,
            exp: now.timestamp() + 9 * 60,
            iss: app_id.to_string(),
        }
    }
}

/// App credentials. `Debug` never prints the key.
pub struct AppAuth {
    app_id: String,
    private_key_pem: String,
}

impl std::fmt::Debug for AppAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppAuth")
            .field("app_id", &self.app_id)
            .field("private_key_pem", &"[REDACTED]")
            .finish()
    }
}

impl AppAuth {
    pub fn new(app_id: String, private_key_pem: String) -> Self {
        AppAuth {
            app_id,
            private_key_pem,
        }
    }

    /// Sign the App JWT (RS256).
    pub fn jwt(&self, now: DateTime<Utc>) -> Result<String, GitHubError> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(self.private_key_pem.as_bytes())
            .map_err(|e| GitHubError::Auth(format!("bad private key: {e}")))?;
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &AppJwtClaims::new(&self.app_id, now),
            &key,
        )
        .map_err(|e| GitHubError::Auth(format!("jwt signing: {e}")))
    }

    /// Exchange the App JWT for an installation token (one network call; the
    /// token is returned to the caller and never stored here).
    pub async fn installation_token(
        &self,
        client: &reqwest::Client,
        base_url: &str,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<String, GitHubError> {
        let jwt = self.jwt(now)?;
        let response = client
            .post(format!(
                "{base_url}/app/installations/{installation_id}/access_tokens"
            ))
            .header("authorization", format!("Bearer {jwt}"))
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "merge0")
            .send()
            .await
            .map_err(|e| GitHubError::Transport(e.to_string()))?;
        let status = response.status();
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| GitHubError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(GitHubError::Api {
                status: status.as_u16(),
                message: value["message"].as_str().unwrap_or("unknown").to_string(),
            });
        }
        value["token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| GitHubError::Auth("no token in response".into()))
    }
}

/// Where API calls get their bearer token. The production impl mints a fresh
/// installation token; tests use a static string.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<String, GitHubError>;
}

/// Static token source for tests and short-lived CLI use.
pub struct StaticToken(pub String);

#[async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String, GitHubError> {
        Ok(self.0.clone())
    }
}

/// Production token source: mint per call sequence via the App flow.
pub struct InstallationTokenSource {
    pub auth: AppAuth,
    pub client: reqwest::Client,
    pub base_url: String,
    pub installation_id: u64,
}

#[async_trait]
impl TokenSource for InstallationTokenSource {
    async fn token(&self) -> Result<String, GitHubError> {
        self.auth
            .installation_token(
                &self.client,
                &self.base_url,
                self.installation_id,
                Utc::now(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn test_rsa_pem() -> String {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let mut rng = rand::thread_rng();
        let key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .unwrap()
            .to_string()
    }

    #[test]
    fn claims_are_backdated_and_capped() {
        let now = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();
        let claims = AppJwtClaims::new("12345", now);
        assert_eq!(claims.iat, now.timestamp() - 60);
        assert_eq!(claims.exp, now.timestamp() + 540);
        assert_eq!(claims.iss, "12345");
        assert!(
            claims.exp - claims.iat <= 600,
            "GitHub caps App JWTs at 10m"
        );
    }

    #[test]
    fn jwt_signs_with_rs256_and_round_trips() {
        let pem = test_rsa_pem();
        let auth = AppAuth::new("777".into(), pem.clone());
        let now = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap();
        let jwt = auth.jwt(now).unwrap();

        // Verify with the matching public key.
        use rsa::pkcs1::DecodeRsaPrivateKey;
        use rsa::pkcs1::EncodeRsaPublicKey;
        let private = rsa::RsaPrivateKey::from_pkcs1_pem(&pem).unwrap();
        let public_pem = private
            .to_public_key()
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .unwrap();
        let decoding = jsonwebtoken::DecodingKey::from_rsa_pem(public_pem.as_bytes()).unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iss"]);
        validation.set_issuer(&["777"]);
        validation.validate_exp = false; // fixed test clock
        let decoded =
            jsonwebtoken::decode::<serde_json::Value>(&jwt, &decoding, &validation).unwrap();
        assert_eq!(decoded.claims["iss"], "777");
    }

    #[test]
    fn debug_redacts_private_key() {
        let auth = AppAuth::new("1".into(), "PRIVATE KEY MATERIAL".into());
        let debug = format!("{auth:?}");
        assert!(!debug.contains("MATERIAL"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn bad_key_is_an_error_not_a_panic() {
        let auth = AppAuth::new("1".into(), "not a pem".into());
        assert!(matches!(auth.jwt(Utc::now()), Err(GitHubError::Auth(_))));
    }
}
