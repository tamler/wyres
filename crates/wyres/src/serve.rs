//! `wyres serve`: the local daemon. Serves MCP over stdio or loopback streamable HTTP and
//! runs the sweeper that expires lapsed holds and marks overdue requests stale.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;
use wyres_core::Store;

use crate::mcp::WyresMcp;

pub const DEFAULT_BIND: &str = "127.0.0.1:7466";

/// Resolve a bind address and refuse anything that is not loopback.
/// Milestone 1 is local-only; remote access needs auth that does not exist yet.
pub fn local_bind(addr: &str) -> Result<SocketAddr> {
    let resolved: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .with_context(|| format!("invalid bind address {addr:?} (expected host:port)"))?
        .collect();
    match resolved.iter().find(|a| a.ip().is_loopback()) {
        Some(a) if resolved.iter().all(|a| a.ip().is_loopback()) => Ok(*a),
        _ => bail!(
            "refusing to bind {addr}: wyres only serves on loopback (127.0.0.1 / ::1 / localhost) \
             until authenticated remote access ships"
        ),
    }
}

/// Periodically expire holds and stale requests until cancelled.
pub fn spawn_sweeper(store: Store, every: Duration, ct: CancellationToken) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tokio::select! {
                _ = ct.cancelled() => break,
                _ = tick.tick() => {
                    let store = store.clone();
                    match tokio::task::spawn_blocking(move || store.sweep()).await {
                        Ok(Ok(r)) if r.expired_holds + r.stale_requests > 0 => tracing::info!(
                            expired_holds = r.expired_holds,
                            stale_requests = r.stale_requests,
                            "sweep"
                        ),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "sweep failed"),
                        Err(e) => tracing::warn!(error = %e, "sweep task failed"),
                    }
                }
            }
        }
    });
}

pub async fn serve_stdio(store: Store, sweep_every: Duration) -> Result<()> {
    let ct = CancellationToken::new();
    spawn_sweeper(store.clone(), sweep_every, ct.clone());
    tracing::info!(node_id = store.node_id(), "serving MCP on stdio");
    let service = WyresMcp::new(store)
        .serve(rmcp::transport::stdio())
        .await
        .context("starting stdio MCP server")?;
    service.waiting().await?;
    ct.cancel();
    Ok(())
}

/// Build the HTTP app: MCP at `/mcp`, plus `GET /health`.
pub fn http_router(store: Store, ct: CancellationToken) -> axum::Router {
    let factory_store = store.clone();
    let mcp: StreamableHttpService<WyresMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(WyresMcp::new(factory_store.clone())),
        Default::default(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );
    let node_id = store.node_id().to_string();
    axum::Router::new().nest_service("/mcp", mcp).route(
        "/health",
        axum::routing::get(move || {
            let node_id = node_id.clone();
            async move { axum::Json(serde_json::json!({ "ok": true, "node_id": node_id })) }
        }),
    )
}

pub async fn serve_http(store: Store, bind: &str, sweep_every: Duration) -> Result<()> {
    let addr = local_bind(bind)?;
    let ct = CancellationToken::new();
    spawn_sweeper(store.clone(), sweep_every, ct.clone());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr()?;
    eprintln!(
        "wyres: MCP (streamable HTTP) on http://{local}/mcp — node {}",
        store.node_id()
    );
    let app = http_router(store, ct.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_binds_are_allowed() {
        assert!(local_bind("127.0.0.1:0").is_ok());
        assert!(local_bind("[::1]:0").is_ok());
        assert!(local_bind("localhost:0").is_ok());
        assert!(local_bind("0.0.0.0:7466").is_err());
        assert!(local_bind("[::]:7466").is_err());
        assert!(local_bind("192.168.1.10:7466").is_err());
        assert!(local_bind("not an address").is_err());
    }
}
