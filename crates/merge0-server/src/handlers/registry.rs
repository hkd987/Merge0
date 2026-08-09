//! The curated skill registry's HTTP face (PRD §5b, P2):
//! `GET /registry/skills` lists the signed index; `POST
//! /registry/skills/{name}/install` opens the manifest-change PR.
//!
//! Trust model: the index ships as a detached-signature JSON file
//! (`index.json` = serialized `SignedIndex`) verified against the pinned
//! public key on EVERY read — a tampered index fails loudly, never
//! partially. Installs verify package content against the listing's pinned
//! hash and apply the curation gate (unproven skills require an explicit
//! `allow_unproven`, which the PR body then discloses to the reviewer).

use super::ApiError;
use crate::{AppState, RegistryHandle};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use merge0_registry::{install_plan, verify_index, RegistryIndex, SignedIndex, SkillPackage};
use serde::Deserialize;

fn handle(state: &AppState) -> Result<&RegistryHandle, ApiError> {
    state.registry.as_deref().ok_or(ApiError::Status(
        StatusCode::SERVICE_UNAVAILABLE,
        "registry not configured".into(),
    ))
}

/// Load + verify the signed index from disk. The file is
/// `{dir}/index.json`: `{"index_json": "...", "signature_hex": "..."}`.
fn load_index(handle: &RegistryHandle) -> Result<RegistryIndex, ApiError> {
    let path = handle.dir.join("index.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("registry index unreadable: {e}")))?;
    let signed: SignedFile = serde_json::from_str(&raw)
        .map_err(|e| ApiError::internal(format!("registry index malformed: {e}")))?;
    verify_index(
        &SignedIndex {
            index_json: signed.index_json,
            signature_hex: signed.signature_hex,
        },
        &handle.verifying_key,
    )
    .map_err(|e| ApiError::conflict(format!("registry index rejected: {e}")))
}

#[derive(Deserialize)]
struct SignedFile {
    index_json: String,
    signature_hex: String,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let index = load_index(handle(&state)?)?;
    Ok(Json(serde_json::json!({
        "generated_at": index.generated_at,
        "skills": index.listings,
    })))
}

#[derive(Deserialize, Default)]
pub struct InstallBody {
    /// Curation-gate override; disclosed in the PR body when used.
    #[serde(default)]
    pub allow_unproven: bool,
}

pub async fn install(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    body: Option<Json<InstallBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let handle = handle(&state)?;
    let index = load_index(handle)?;
    let Some(listing) = index.listings.iter().find(|l| l.name == name) else {
        return Err(ApiError::not_found(format!("no skill named {name:?}")));
    };

    // Package payload: `{dir}/skills/{name}/**` in sorted path order (the
    // canonical order the content hash was computed over).
    let package_dir = handle.dir.join("skills").join(&name);
    let mut files = Vec::new();
    collect_files(&package_dir, &package_dir, &mut files)
        .map_err(|e| ApiError::internal(format!("skill package unreadable: {e}")))?;
    files.sort();
    let package = SkillPackage {
        listing: listing.clone(),
        files,
    };

    // The customer's live manifest, verbatim — install_plan preserves it.
    let existing_manifest = state
        .github
        .get_file_content(&state.repo, merge0_runner::manifest::MANIFEST_PATH)
        .await
        .map_err(|e| ApiError::internal(format!("manifest fetch failed: {e}")))?
        .unwrap_or_default();

    let allow_unproven = body.map(|b| b.allow_unproven).unwrap_or(false);
    let plan = install_plan(
        &package,
        &existing_manifest,
        &state.repo.full(),
        allow_unproven,
    )
    .map_err(|e| ApiError::conflict(e.to_string()))?;

    state
        .github
        .create_branch_with_files(&state.repo, &plan.branch_name, &plan.files, &plan.pr_title)
        .await
        .map_err(|e| ApiError::internal(format!("install branch failed: {e}")))?;
    let base = state
        .github
        .default_branch(&state.repo)
        .await
        .map_err(|e| ApiError::internal(format!("default branch lookup failed: {e}")))?;
    let pr = state
        .github
        .create_pull_request(
            &state.repo,
            &plan.branch_name,
            &base,
            &plan.pr_title,
            &plan.pr_body,
        )
        .await
        .map_err(|e| ApiError::internal(format!("install PR failed: {e}")))?;

    Ok(Json(serde_json::json!({
        "installed": name,
        "pr_url": pr.url,
        "branch": plan.branch_name,
        "allow_unproven": allow_unproven,
    })))
}

fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("walked paths stay under root")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, std::fs::read_to_string(&path)?));
        }
    }
    Ok(())
}
