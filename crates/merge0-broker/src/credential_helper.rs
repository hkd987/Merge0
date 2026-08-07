//! Git credential helper wire protocol (PRD §5a).
//!
//! Delivering the token through a credential helper is the mechanism that
//! keeps the raw value out of the agent's transcript and logs: git invokes
//! the helper, the helper asks the broker, and the token flows straight
//! back to git over stdout — never through the agent's prompt context and
//! never onto a command line. The wire format is git's `key=value` lines
//! terminated by a blank line (`git-credential(1)`).

use crate::{BrokerError, SecretToken};

/// The fixed username GitHub expects when the password is an App
/// installation token.
pub const CREDENTIAL_USERNAME: &str = "x-access-token";

/// A parsed `git credential fill` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
    pub protocol: String,
    pub host: String,
    /// Present when git is configured with `credential.useHttpPath` —
    /// which broker deployments should require, since the path is what
    /// lets the helper ask for the right single-repo token.
    pub path: Option<String>,
}

/// Parse the `key=value` lines git writes to the helper's stdin.
///
/// Unknown keys are ignored (git may send `username`, `wwwauth[]`, etc.);
/// a blank line ends the request. `protocol` and `host` are required.
/// Malformed input returns [`BrokerError::MalformedCredentialRequest`] —
/// never a panic, since this text arrives from outside the trust boundary.
pub fn parse_request(stdin_text: &str) -> Result<CredentialRequest, BrokerError> {
    let mut protocol = None;
    let mut host = None;
    let mut path = None;
    for line in stdin_text.lines() {
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(BrokerError::MalformedCredentialRequest(format!(
                "line without '=': {line}"
            )));
        };
        match key {
            "protocol" => protocol = Some(value.to_string()),
            "host" => host = Some(value.to_string()),
            "path" => path = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(CredentialRequest {
        protocol: protocol.ok_or_else(|| {
            BrokerError::MalformedCredentialRequest("missing required key: protocol".into())
        })?,
        host: host.ok_or_else(|| {
            BrokerError::MalformedCredentialRequest("missing required key: host".into())
        })?,
        path,
    })
}

/// Format the helper's stdout response for git.
///
/// This is the sole intended consumer of
/// [`SecretToken::expose_for_credential_helper`]: the returned string goes
/// to git over a pipe and nowhere else. Callers must not log it. Use
/// [`CREDENTIAL_USERNAME`] unless the remote requires otherwise.
pub fn format_response(username: &str, token: &SecretToken) -> String {
    format!(
        "username={username}\npassword={}\n\n",
        token.expose_for_credential_helper()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_the_wire_format() {
        let wire = "protocol=https\nhost=github.example.com\npath=example-org/example-repo.git\n\n";
        let request = parse_request(wire).unwrap();
        assert_eq!(request.protocol, "https");
        assert_eq!(request.host, "github.example.com");
        assert_eq!(
            request.path.as_deref(),
            Some("example-org/example-repo.git")
        );
    }

    #[test]
    fn unknown_keys_are_ignored_and_path_is_optional() {
        let wire = "protocol=https\nhost=github.example.com\nusername=git\nwwwauth[]=Basic\n\n";
        let request = parse_request(wire).unwrap();
        assert_eq!(request.path, None);
        assert_eq!(request.host, "github.example.com");
    }

    #[test]
    fn missing_required_keys_and_garbage_are_typed_errors() {
        assert!(matches!(
            parse_request("host=github.example.com\n\n"),
            Err(BrokerError::MalformedCredentialRequest(msg)) if msg.contains("protocol")
        ));
        assert!(matches!(
            parse_request("protocol=https\n\n"),
            Err(BrokerError::MalformedCredentialRequest(msg)) if msg.contains("host")
        ));
        assert!(matches!(
            parse_request("this is not a credential request"),
            Err(BrokerError::MalformedCredentialRequest(_))
        ));
    }

    #[test]
    fn response_carries_the_raw_token_exactly_once_for_git() {
        let token = SecretToken::new("fake-token-value");
        let response = format_response(CREDENTIAL_USERNAME, &token);
        assert_eq!(
            response,
            "username=x-access-token\npassword=fake-token-value\n\n"
        );
        // And the token stays redacted everywhere except that response.
        assert_eq!(format!("{token:?}"), "[REDACTED]");
    }
}
