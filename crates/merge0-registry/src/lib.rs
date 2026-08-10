//! Curated signed skill registry (PRD §5b, P2).
//!
//! The trust model for agent configuration is identical to the trust model
//! for customer code: nothing takes effect except through the customer's
//! own PR review. This crate encodes the three mechanisms that make a
//! curated registry safe to offer at all:
//!
//! 1. **Signed index.** The registry index is serialized to canonical JSON
//!    and ed25519-signed by Merge0 ([`sign_index`]). Clients verify with a
//!    pinned public key ([`verify_index`]) before trusting a single byte of
//!    listing metadata. Verification also rejects any index whose listings
//!    lack content hashes — an unhashed listing is uninstallable by
//!    construction, closing the "signed index, unpinned payload" gap.
//! 2. **Content-hashed packages.** Every skill package must hash to the
//!    `content_sha256` its listing advertised ([`SkillPackage::verify_content`]),
//!    so a compromised mirror cannot swap file contents under a valid index.
//!    Unvetted skills are prompt-injection vectors into an agent that
//!    writes code (PRD Risks table); the hash chain is the mechanical half
//!    of the "curated and signed" mitigation.
//! 3. **Install = manifest-change PR.** [`install_plan`] produces a branch,
//!    files, and PR copy — never a server-side toggle. Skill files land
//!    under `.merge0/skills/{name}/` and the `.merge0/agent.toml` update is
//!    a real TOML parse that preserves every existing customer entry
//!    (`[[mcp]]`, `[network]`, comments excepted) and only ensures the
//!    `[skills]` table exists.
//!
//! **Curation gate:** listings are gated on acceptance telemetry (PRD §5b:
//! "listings carry per-skill acceptance-rate telemetry"). A listing with no
//! telemetry, or zero recorded runs, is refused by [`install_plan`] unless
//! the caller passes `allow_unproven = true` — an explicit, auditable
//! override, surfaced in the PR body so the reviewing human sees it too.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, Verifier};
// Re-exported so registry consumers (the server's HTTP surface) never take
// a direct ed25519 dependency.
pub use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Directory (in the customer repo) where registry-installed skills land.
pub const SKILLS_DIR: &str = ".merge0/skills/";
/// The agent manifest path in the customer repo (PRD §5b).
pub const MANIFEST_PATH: &str = ".merge0/agent.toml";

/// Typed errors — registry indexes and packages arrive from outside the
/// trust boundary, so nothing here panics on bad input.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("serialization failed: {0}")]
    Serialize(String),
    #[error("signature is not valid hex-encoded ed25519")]
    BadSignatureEncoding,
    #[error("index signature verification failed")]
    SignatureInvalid,
    #[error("signed index payload is not a valid registry index: {0}")]
    MalformedIndex(String),
    #[error("listing {name} lacks a valid content_sha256 (64 hex chars required)")]
    MissingContentHash { name: String },
    #[error("package content hash mismatch: listing says {expected}, files hash to {actual}")]
    ContentHashMismatch { expected: String, actual: String },
    #[error(
        "skill {name} has no acceptance telemetry (or zero runs); \
         pass allow_unproven=true to install anyway"
    )]
    UnprovenSkill { name: String },
    #[error("could not parse existing agent manifest: {0}")]
    ManifestParse(String),
}

/// Per-skill acceptance telemetry (PRD §5b: extension quality is
/// *measurable* — "PRs run with the DB MCP merge at 80%; without, 55%").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceptanceTelemetry {
    /// Work Orders that ran with this skill active.
    pub runs: u64,
    /// Fraction of those runs whose PR merged, in `0.0..=1.0`.
    pub merge_rate: f64,
}

/// One curated skill as advertised by the registry index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillListing {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Hash of the package contents (see [`content_hash`] for the canonical
    /// construction). Lower-case hex, 64 chars.
    pub content_sha256: String,
    /// `None` until the skill has accumulated outcome telemetry; unproven
    /// listings are blocked by the curation gate in [`install_plan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<AcceptanceTelemetry>,
}

/// The full registry index, signed as one unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryIndex {
    pub generated_at: DateTime<Utc>,
    pub listings: Vec<SkillListing>,
}

/// A signed index as distributed to clients. `index_json` is the exact
/// canonical byte string that was signed — clients verify *those bytes*,
/// then parse, so there is no canonicalization gap between what was signed
/// and what is trusted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedIndex {
    pub index_json: String,
    pub signature_hex: String,
}

/// Sign an index with Merge0's registry key. The canonical form is
/// `serde_json::to_string` of the index; that exact string is what ships
/// and what [`verify_index`] checks.
pub fn sign_index(
    index: &RegistryIndex,
    signing_key: &SigningKey,
) -> Result<SignedIndex, RegistryError> {
    let index_json =
        serde_json::to_string(index).map_err(|e| RegistryError::Serialize(e.to_string()))?;
    let signature = signing_key.sign(index_json.as_bytes());
    Ok(SignedIndex {
        index_json,
        signature_hex: hex::encode(signature.to_bytes()),
    })
}

/// Verify a signed index against Merge0's pinned public key and return the
/// parsed index.
///
/// Rejects: bad signature encoding, signature mismatch (any flipped byte in
/// `index_json`), unparseable payloads, and any index containing a listing
/// without a well-formed `content_sha256` — the index is only useful as a
/// trust root if every listing pins its content.
pub fn verify_index(
    signed: &SignedIndex,
    verifying_key: &VerifyingKey,
) -> Result<RegistryIndex, RegistryError> {
    let sig_bytes =
        hex::decode(&signed.signature_hex).map_err(|_| RegistryError::BadSignatureEncoding)?;
    let signature =
        Signature::from_slice(&sig_bytes).map_err(|_| RegistryError::BadSignatureEncoding)?;
    verifying_key
        .verify(signed.index_json.as_bytes(), &signature)
        .map_err(|_| RegistryError::SignatureInvalid)?;
    let index: RegistryIndex = serde_json::from_str(&signed.index_json)
        .map_err(|e| RegistryError::MalformedIndex(e.to_string()))?;
    for listing in &index.listings {
        if !is_sha256_hex(&listing.content_sha256) {
            return Err(RegistryError::MissingContentHash {
                name: listing.name.clone(),
            });
        }
    }
    Ok(index)
}

/// Parse a hex-encoded ed25519 public key (the pinned registry trust root,
/// as operators configure it via env).
pub fn verifying_key_from_hex(hex_key: &str) -> Result<VerifyingKey, RegistryError> {
    let bytes = hex::decode(hex_key.trim()).map_err(|_| RegistryError::BadSignatureEncoding)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| RegistryError::BadSignatureEncoding)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| RegistryError::BadSignatureEncoding)
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Canonical package content hash.
///
/// For each `(path, content)` pair **in the order given**, the SHA-256
/// state is updated with the path bytes, a `0x00` separator, the content
/// bytes, and another `0x00`. The separators prevent path/content and
/// cross-file concatenation ambiguity (`("a", "bc")` vs `("ab", "c")`);
/// order sensitivity means registry packaging must emit files in a fixed
/// (sorted) order, which the packaging side owns.
pub fn content_hash(files: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    for (path, content) in files {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(content.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

/// A skill package as fetched for installation: the listing plus the
/// actual files, `(relative path, content)`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillPackage {
    pub listing: SkillListing,
    pub files: Vec<(String, String)>,
}

impl SkillPackage {
    /// Check the package files against the listing's pinned content hash
    /// (see [`content_hash`] for the canonical construction).
    pub fn verify_content(&self) -> Result<(), RegistryError> {
        let actual = content_hash(&self.files);
        if actual != self.listing.content_sha256 {
            return Err(RegistryError::ContentHashMismatch {
                expected: self.listing.content_sha256.clone(),
                actual,
            });
        }
        Ok(())
    }
}

/// Everything needed to open the install PR against the customer repo.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallPlan {
    pub branch_name: String,
    /// `(repo-relative path, content)`: the skill files under
    /// `.merge0/skills/{name}/` plus the updated `.merge0/agent.toml`.
    pub files: Vec<(String, String)>,
    pub pr_title: String,
    pub pr_body: String,
}

/// Build the manifest-change PR that installs a skill (PRD §5b P2:
/// "install writes a manifest-change PR to the customer's repo — never a
/// silent server-side toggle").
///
/// Verifies package content against the listing hash first, then applies
/// the **curation gate**: a listing whose `acceptance` is `None` or whose
/// `runs == 0` has no outcome evidence behind it and is refused unless
/// `allow_unproven = true` is passed explicitly. The override exists for
/// design partners dogfooding new skills; it is recorded in the PR body so
/// the human reviewing the PR sees the skill is unproven.
///
/// The manifest update parses the customer's existing `agent.toml` with the
/// `toml` crate (no string-mangling), preserves every existing entry, and
/// only ensures a `[skills]` table with a `path` key exists — an existing
/// customer-set `skills.path` is left untouched.
pub fn install_plan(
    package: &SkillPackage,
    existing_manifest_toml: &str,
    repo: &str,
    allow_unproven: bool,
) -> Result<InstallPlan, RegistryError> {
    package.verify_content()?;

    let listing = &package.listing;
    let proven = matches!(&listing.acceptance, Some(t) if t.runs > 0);
    if !proven && !allow_unproven {
        return Err(RegistryError::UnprovenSkill {
            name: listing.name.clone(),
        });
    }

    let updated_manifest = ensure_skills_table(existing_manifest_toml)?;

    let mut files: Vec<(String, String)> = package
        .files
        .iter()
        .map(|(path, content)| {
            (
                format!("{SKILLS_DIR}{}/{path}", listing.name),
                content.clone(),
            )
        })
        .collect();
    files.push((MANIFEST_PATH.to_string(), updated_manifest));

    let telemetry_line = match &listing.acceptance {
        Some(t) if t.runs > 0 => format!(
            "Acceptance telemetry: {} runs, {:.1}% merge rate.",
            t.runs,
            t.merge_rate * 100.0
        ),
        _ => "Acceptance telemetry: none yet — installed with `allow_unproven` \
             explicitly set."
            .to_string(),
    };

    let pr_body = format!(
        "Installs the Merge0-curated skill **{name}** v{version} into `{repo}`.\n\n\
         {description}\n\n\
         - Skill: `{name}` v{version}\n\
         - Content SHA-256: `{hash}`\n\
         - {telemetry}\n\n\
         Files land under `{skills_dir}{name}/`; `{manifest}` gains a `[skills]` \
         table (existing manifest entries are preserved). Nothing takes effect \
         until this PR is reviewed and merged — registry installs are always \
         delivered as manifest-change PRs, never server-side toggles.",
        name = listing.name,
        version = listing.version,
        repo = repo,
        description = listing.description,
        hash = listing.content_sha256,
        telemetry = telemetry_line,
        skills_dir = SKILLS_DIR,
        manifest = MANIFEST_PATH,
    );

    Ok(InstallPlan {
        branch_name: format!("merge0/skill-{}-{}", listing.name, listing.version),
        files,
        pr_title: format!("Install Merge0 skill {} v{}", listing.name, listing.version),
        pr_body,
    })
}

/// Parse the manifest and ensure `[skills]` with a `path` key exists,
/// preserving everything else. Comments are not preserved (the `toml`
/// value model drops them); table contents and ordering semantics are.
fn ensure_skills_table(existing: &str) -> Result<String, RegistryError> {
    let mut manifest: toml::Table = existing
        .parse()
        .map_err(|e: toml::de::Error| RegistryError::ManifestParse(e.to_string()))?;
    let skills = manifest
        .entry("skills")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let skills_table = skills.as_table_mut().ok_or_else(|| {
        RegistryError::ManifestParse("`skills` key exists but is not a table".into())
    })?;
    skills_table
        .entry("path")
        .or_insert_with(|| toml::Value::String(SKILLS_DIR.to_string()));
    toml::to_string_pretty(&manifest).map_err(|e| RegistryError::Serialize(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn keypair() -> (SigningKey, VerifyingKey) {
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    fn fixture_files() -> Vec<(String, String)> {
        vec![
            (
                "SKILL.md".to_string(),
                "# House migration style\nAlways write reversible migrations.".to_string(),
            ),
            (
                "examples/reversible.sql".to_string(),
                "-- example content".to_string(),
            ),
        ]
    }

    fn fixture_listing(acceptance: Option<AcceptanceTelemetry>) -> SkillListing {
        SkillListing {
            name: "house-migrations".to_string(),
            version: "1.2.0".to_string(),
            description: "House style for schema migrations".to_string(),
            content_sha256: content_hash(&fixture_files()),
            acceptance,
        }
    }

    fn proven() -> Option<AcceptanceTelemetry> {
        Some(AcceptanceTelemetry {
            runs: 120,
            merge_rate: 0.8,
        })
    }

    fn fixture_index() -> RegistryIndex {
        RegistryIndex {
            generated_at: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            listings: vec![fixture_listing(proven())],
        }
    }

    /// Customer manifest with the PRD §5b example shapes: MCP entries and a
    /// network egress allowlist that installation must preserve.
    const FIXTURE_MANIFEST: &str = r#"
[[mcp]]
name = "internal-api"
command = "npx example-api-mcp"
auth_env = "INTERNAL_API_KEY"

[[mcp]]
name = "db-schema"
command = "npx example-db-mcp"
auth_env = "DB_SCHEMA_KEY"

[network]
egress_allow = ["api.internal.example.com"]
"#;

    #[test]
    fn sign_verify_round_trip() {
        let (signing, verifying) = keypair();
        let index = fixture_index();
        let signed = sign_index(&index, &signing).unwrap();
        let verified = verify_index(&signed, &verifying).unwrap();
        assert_eq!(verified, index);
    }

    #[test]
    fn tampered_index_is_rejected() {
        let (signing, verifying) = keypair();
        let signed = sign_index(&fixture_index(), &signing).unwrap();

        // Flip one byte in the signed JSON (bump the run count).
        let tampered_json = signed.index_json.replacen("120", "121", 1);
        assert_ne!(tampered_json, signed.index_json, "fixture must contain 120");
        let tampered = SignedIndex {
            index_json: tampered_json,
            ..signed.clone()
        };
        assert!(matches!(
            verify_index(&tampered, &verifying),
            Err(RegistryError::SignatureInvalid)
        ));

        // A wrong key fails too, and bad hex is a distinct error.
        let (_, other_key) = keypair();
        assert!(matches!(
            verify_index(&signed, &other_key),
            Err(RegistryError::SignatureInvalid)
        ));
        let bad_hex = SignedIndex {
            signature_hex: "zz".repeat(64),
            ..signed
        };
        assert!(matches!(
            verify_index(&bad_hex, &verifying),
            Err(RegistryError::BadSignatureEncoding)
        ));
    }

    #[test]
    fn index_with_unhashed_listing_is_rejected_even_when_signed() {
        let (signing, verifying) = keypair();
        let mut index = fixture_index();
        index.listings[0].content_sha256 = String::new();
        let signed = sign_index(&index, &signing).unwrap();
        assert!(matches!(
            verify_index(&signed, &verifying),
            Err(RegistryError::MissingContentHash { name }) if name == "house-migrations"
        ));
    }

    #[test]
    fn package_content_verification_passes_and_fails() {
        let package = SkillPackage {
            listing: fixture_listing(proven()),
            files: fixture_files(),
        };
        package.verify_content().unwrap();

        let mut tampered = package;
        tampered.files[0].1.push_str("\ninjected instruction");
        assert!(matches!(
            tampered.verify_content(),
            Err(RegistryError::ContentHashMismatch { .. })
        ));
    }

    #[test]
    fn content_hash_separates_path_and_content_boundaries() {
        let a = content_hash(&[("ab".into(), "c".into())]);
        let b = content_hash(&[("a".into(), "bc".into())]);
        assert_ne!(a, b);
    }

    #[test]
    fn install_plan_preserves_unrelated_manifest_content() {
        let package = SkillPackage {
            listing: fixture_listing(proven()),
            files: fixture_files(),
        };
        let plan = install_plan(
            &package,
            FIXTURE_MANIFEST,
            "example-org/example-repo",
            false,
        )
        .unwrap();

        let (manifest_path, updated) = plan
            .files
            .iter()
            .find(|(path, _)| path == MANIFEST_PATH)
            .expect("plan must include the updated manifest");
        assert_eq!(manifest_path, MANIFEST_PATH);

        // Compare parsed tables: everything the customer had is still there.
        let updated_table: toml::Table = updated.parse().unwrap();
        let original_table: toml::Table = FIXTURE_MANIFEST.parse().unwrap();
        assert_eq!(updated_table["mcp"], original_table["mcp"]);
        assert_eq!(updated_table["network"], original_table["network"]);
        // ...and the skills table now exists with the standard path.
        assert_eq!(updated_table["skills"]["path"].as_str(), Some(SKILLS_DIR));
    }

    #[test]
    fn install_plan_leaves_customer_set_skills_path_untouched() {
        let manifest = "[skills]\npath = \"custom/skills/\"\n";
        let package = SkillPackage {
            listing: fixture_listing(proven()),
            files: fixture_files(),
        };
        let plan = install_plan(&package, manifest, "example-org/example-repo", false).unwrap();
        let updated: toml::Table = plan.files.last().unwrap().1.parse().unwrap();
        assert_eq!(updated["skills"]["path"].as_str(), Some("custom/skills/"));
    }

    #[test]
    fn install_plan_layout_and_pr_copy() {
        let package = SkillPackage {
            listing: fixture_listing(proven()),
            files: fixture_files(),
        };
        let plan = install_plan(&package, "", "example-org/example-repo", false).unwrap();

        assert_eq!(plan.branch_name, "merge0/skill-house-migrations-1.2.0");
        assert_eq!(plan.files[0].0, ".merge0/skills/house-migrations/SKILL.md");
        assert_eq!(
            plan.files[1].0,
            ".merge0/skills/house-migrations/examples/reversible.sql"
        );
        assert_eq!(plan.files[2].0, MANIFEST_PATH);

        assert!(plan.pr_title.contains("house-migrations"));
        assert!(plan.pr_title.contains("1.2.0"));
        assert!(plan.pr_body.contains(&package.listing.content_sha256));
        assert!(plan.pr_body.contains("120 runs"));
        assert!(plan.pr_body.contains("80.0% merge rate"));
        assert!(plan.pr_body.contains("example-org/example-repo"));
    }

    #[test]
    fn curation_gate_blocks_unproven_listings_by_default() {
        for acceptance in [
            None,
            Some(AcceptanceTelemetry {
                runs: 0,
                merge_rate: 0.0,
            }),
        ] {
            let package = SkillPackage {
                listing: fixture_listing(acceptance),
                files: fixture_files(),
            };
            let err = install_plan(&package, "", "example-org/example-repo", false).unwrap_err();
            assert!(matches!(
                err,
                RegistryError::UnprovenSkill { name } if name == "house-migrations"
            ));

            // Explicit override works and is called out in the PR body.
            let plan = install_plan(&package, "", "example-org/example-repo", true).unwrap();
            assert!(plan.pr_body.contains("allow_unproven"));
        }
    }

    #[test]
    fn bad_content_hash_blocks_install_before_the_gate() {
        let mut listing = fixture_listing(proven());
        listing.content_sha256 = "0".repeat(64);
        let package = SkillPackage {
            listing,
            files: fixture_files(),
        };
        assert!(matches!(
            install_plan(&package, "", "example-org/example-repo", false),
            Err(RegistryError::ContentHashMismatch { .. })
        ));
    }

    #[test]
    fn unparseable_manifest_is_a_typed_error() {
        let package = SkillPackage {
            listing: fixture_listing(proven()),
            files: fixture_files(),
        };
        assert!(matches!(
            install_plan(&package, "not = = toml", "example-org/example-repo", false),
            Err(RegistryError::ManifestParse(_))
        ));
    }
}
