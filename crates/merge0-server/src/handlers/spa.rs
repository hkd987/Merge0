//! The web UI: a React SPA built to `ui/dist` and embedded in the binary
//! (`rust-embed`), served on the OPEN router. Pages are data-free static
//! assets — the API token lives in the browser and rides the JSON calls
//! (audit C2) — so serving them unauthenticated leaks nothing.
//!
//! Routing: `/`, `/inbox`, `/dashboard`, `/setup` all serve `index.html`
//! (client-side routing); hashed files under `/assets/` are immutable.
//! If the UI was never built, respond 503 with the fix.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // Real files (hashed bundles, fonts) serve as-is; every app route
    // falls back to the SPA shell.
    if let Some(file) = Assets::get(path) {
        return respond(path, file.data.into_owned(), true);
    }
    match Assets::get("index.html") {
        Some(file) => respond("index.html", file.data.into_owned(), false),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "UI not built — run `npm run build` in ui/ and rebuild the server",
        )
            .into_response(),
    }
}

fn respond(path: &str, data: Vec<u8>, immutable: bool) -> Response {
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    let cache = if immutable && path.starts_with("assets/") {
        // Vite content-hashes bundle names.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)],
        data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn app_routes_fall_back_to_the_shell_or_503() {
        let response = serve(Uri::from_static("/inbox")).await;
        // Built UI → the data-free shell; unbuilt → actionable 503.
        assert!(
            response.status() == StatusCode::OK
                || response.status() == StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
