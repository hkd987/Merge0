//! Service binary stub. The Axum service (ingestion scheduling, triage runs,
//! inbox API) lands in a follow-up branch — this exists so the workspace has
//! its entry point from day one.

fn main() {
    println!("merge0-server {} (stub)", env!("CARGO_PKG_VERSION"));
}
