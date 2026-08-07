//! Hosted control-plane entry point (commercial).

use merge0_ee::TenantManager;
use merge0_hosted::{app, HostedState};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url =
        std::env::var("MERGE0_DATABASE_URL").map_err(|_| "missing MERGE0_DATABASE_URL")?;
    let manager = TenantManager::connect(&database_url).await?;

    let admin_token = std::env::var("MERGE0_EE_ADMIN_TOKEN").ok();
    if admin_token.is_none() {
        tracing::warn!("MERGE0_EE_ADMIN_TOKEN unset — control plane is OPEN (dev only)");
    }

    let state = HostedState {
        manager: Arc::new(manager),
        admin_token,
    };
    let bind = std::env::var("MERGE0_EE_BIND").unwrap_or_else(|_| "127.0.0.1:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("merge0-hosted control plane listening on {bind}");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
