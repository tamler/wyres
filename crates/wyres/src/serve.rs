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

/// HTTP serving options.
#[derive(Debug, Clone, Default)]
pub struct HttpOptions {
    /// Require bearer tokens on `/mcp` and `/activity`.
    pub auth: bool,
    /// Explicitly allow a non-loopback bind (only together with `auth`).
    pub allow_remote_bind: bool,
    /// Extra `Host` header values to accept (tunnel hostnames). Only with `auth`.
    pub allowed_hosts: Vec<String>,
    /// Serve the federation hub endpoints (`/fed/*`), authenticated by node signatures.
    pub federation: bool,
}

/// Resolve a bind address and refuse anything that is not loopback.
pub fn local_bind(addr: &str) -> Result<SocketAddr> {
    check_bind(addr, &HttpOptions::default())
}

/// Resolve a bind address. Loopback is always fine; anything else needs both `--auth` and
/// `--allow-remote-bind`.
pub fn check_bind(addr: &str, opts: &HttpOptions) -> Result<SocketAddr> {
    let resolved: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .with_context(|| format!("invalid bind address {addr:?} (expected host:port)"))?
        .collect();
    let Some(first) = resolved.first().copied() else {
        bail!("bind address {addr:?} did not resolve");
    };
    if resolved.iter().all(|a| a.ip().is_loopback()) {
        return Ok(first);
    }
    if opts.auth && opts.allow_remote_bind {
        return Ok(first);
    }
    bail!(
        "refusing to bind {addr}: wyres serves on loopback (127.0.0.1 / ::1 / localhost) unless \
         both --auth and --allow-remote-bind are given; prefer a tunnel to a loopback --auth server"
    )
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

/// Build the HTTP app: MCP at `/mcp`, the activity feed at `GET /activity`, and
/// `GET /health`. With `auth`, `/mcp` and `/activity` require a bearer token.
pub fn http_router(store: Store, ct: CancellationToken, opts: &HttpOptions) -> axum::Router {
    let factory_store = store.clone();
    let mut config =
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token());
    if opts.auth && !opts.allowed_hosts.is_empty() {
        let mut hosts = config.allowed_hosts.clone();
        hosts.extend(opts.allowed_hosts.iter().cloned());
        config = config.with_allowed_hosts(hosts);
    }
    let mcp: StreamableHttpService<WyresMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(WyresMcp::new(factory_store.clone())),
        Default::default(),
        config,
    );
    let protected = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/activity", axum::routing::get(crate::remote::activity))
        .with_state(store.clone());
    let protected = if opts.auth {
        protected.layer(axum::middleware::from_fn_with_state(
            store.clone(),
            crate::remote::require_token,
        ))
    } else {
        protected
    };
    let protected = if opts.federation {
        protected.merge(crate::fed::router(store.clone()))
    } else {
        protected
    };
    let node_id = store.node_id().to_string();
    let auth = opts.auth;
    protected.route(
        "/health",
        axum::routing::get(move || {
            let node_id = node_id.clone();
            async move {
                axum::Json(serde_json::json!({ "ok": true, "node_id": node_id, "auth": auth }))
            }
        }),
    )
}

pub async fn serve_http(
    store: Store,
    bind: &str,
    opts: HttpOptions,
    sweep_every: Duration,
) -> Result<()> {
    let addr = check_bind(bind, &opts)?;
    let mut opts = opts;
    if !addr.ip().is_loopback() {
        // Accept requests addressed to the bound IP itself.
        opts.allowed_hosts.push(addr.ip().to_string());
    }
    let ct = CancellationToken::new();
    spawn_sweeper(store.clone(), sweep_every, ct.clone());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr()?;
    eprintln!(
        "wyres: MCP (streamable HTTP) on http://{local}/mcp{} — node {}",
        if opts.auth {
            " (bearer token required)"
        } else {
            ""
        },
        store.node_id()
    );
    let app = http_router(store, ct.clone(), &opts);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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

    #[test]
    fn remote_binds_need_auth_and_explicit_flag() {
        let auth_only = HttpOptions {
            auth: true,
            ..Default::default()
        };
        assert!(check_bind("0.0.0.0:0", &auth_only).is_err());
        let flag_only = HttpOptions {
            allow_remote_bind: true,
            ..Default::default()
        };
        assert!(check_bind("0.0.0.0:0", &flag_only).is_err());
        let both = HttpOptions {
            auth: true,
            allow_remote_bind: true,
            ..Default::default()
        };
        assert!(check_bind("0.0.0.0:0", &both).is_ok());
        assert!(check_bind("127.0.0.1:0", &auth_only).is_ok());
    }
}
