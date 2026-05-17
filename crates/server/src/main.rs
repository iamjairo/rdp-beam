mod auth;
mod config;
mod session;
mod signaling;
mod tls;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::session::SessionManager;
use crate::web::AppState;

fn parse_args() -> (PathBuf, Option<u16>) {
    let args: Vec<String> = std::env::args().collect();
    let mut config_path = PathBuf::from("./config/beam.toml");
    let mut port_override = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-V" | "--version" => {
                println!("beam-server {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("beam-server - Beam Remote Desktop signaling server");
                println!();
                println!("USAGE:");
                println!("    beam-server [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!(
                    "    -c, --config <PATH>    Configuration file [default: ./config/beam.toml]"
                );
                println!("    -p, --port <PORT>      Override server port");
                println!("    -V, --version          Print version and exit");
                println!("    -h, --help             Print this help and exit");
                std::process::exit(0);
            }
            "--config" | "-c" if i + 1 < args.len() => {
                config_path = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            "--port" | "-p" if i + 1 < args.len() => {
                port_override = args[i + 1].parse().ok();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    (config_path, port_override)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (config_path, port_override) = parse_args();

    // Load configuration
    let mut config = config::load_config(&config_path)?;
    if let Some(p) = port_override {
        config.server.port = p;
    }
    // Validate configuration semantics
    if let Err(issues) = config.validate() {
        let has_errors = issues.iter().any(|i| i.starts_with("ERROR:"));
        for issue in &issues {
            if issue.starts_with("ERROR:") {
                tracing::error!("{}", issue);
            } else {
                tracing::warn!("{}", issue);
            }
        }
        if has_errors {
            tracing::error!(
                "Configuration has {} issue(s). Fix the ERROR(s) above and restart.",
                issues.len()
            );
            std::process::exit(1);
        }
    }

    // Validate PAM config exists (required for systemd-logind session registration)
    if !std::path::Path::new("/etc/pam.d/beam").exists() {
        tracing::warn!(
            "PAM config /etc/pam.d/beam not found — agent sessions will fail to start. \
             Install the beam package or copy packaging/pam.d/beam to /etc/pam.d/beam."
        );
    }

    // Validate web root exists so we don't silently serve 404
    if !std::path::Path::new(&config.server.web_root).is_dir() {
        tracing::warn!(
            "Web root '{}' does not exist — the UI will not load. \
             Build with 'make build-web' or set server.web_root in the config.",
            config.server.web_root
        );
    }

    let port = config.server.port;
    let bind_addr: SocketAddr = format!("{}:{}", config.server.bind, port)
        .parse()
        .context("Invalid bind address")?;

    // Build TLS config
    let tls_result = tls::build_tls_config(
        config.server.tls_cert.as_deref(),
        config.server.tls_key.as_deref(),
    )?;
    let tls_acceptor = tls::make_acceptor(tls_result.config);
    let tls_cert_path = tls_result.cert_pem_path;

    // JWT secret — persist to /var/lib/beam/jwt_secret so tokens survive restarts
    let jwt_secret = config.server.jwt_secret.clone().unwrap_or_else(|| {
        let secret_path = std::path::Path::new("/var/lib/beam/jwt_secret");
        // Try to read existing persisted secret
        if let Ok(existing) = std::fs::read_to_string(secret_path) {
            let trimmed = existing.trim().to_string();
            if !trimmed.is_empty() {
                tracing::info!("Loaded JWT secret from {}", secret_path.display());
                return trimmed;
            }
        }
        // Generate and persist a new secret
        let secret = auth::generate_secret();
        if let Err(e) = std::fs::create_dir_all("/var/lib/beam") {
            tracing::warn!("Failed to create /var/lib/beam: {e}");
        } else {
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(secret_path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = f.write_all(secret.as_bytes());
                    tracing::info!("Persisted JWT secret to {}", secret_path.display());
                }
                Err(e) => {
                    tracing::warn!("Failed to persist JWT secret: {e}");
                }
            }
        }
        secret
    });

    // Session manager
    let session_manager = SessionManager::new(
        config.session.display_start,
        config.session.default_width,
        config.session.default_height,
        Some(tls_cert_path),
        config.video.clone(),
        config.session.gpu_driver.clone(),
        config.session.backend.clone(),
    );

    // Build app state and router
    let state = Arc::new(AppState {
        config,
        session_manager,
        channels: signaling::new_channel_registry(),
        jwt_secret,
        login_limiter: web::LoginRateLimiter::new(5, 60), // 5 attempts per username per 60s
        ip_limiter: web::LoginRateLimiter::new(20, 60),   // 20 attempts per IP per 60s
        release_limiter: web::LoginRateLimiter::new(10, 60), // 10 release attempts per IP per 60s
        started_at: std::time::Instant::now(),
        metrics_logins_attempted: std::sync::atomic::AtomicU64::new(0),
        metrics_logins_failed: std::sync::atomic::AtomicU64::new(0),
        metrics_agent_restarts: std::sync::atomic::AtomicU64::new(0),
    });

    // Restore sessions from previous graceful shutdown
    let restored = state.session_manager.restore_sessions().await;
    for session_id in &restored {
        signaling::get_or_create_channel(&state.channels, *session_id).await;
        web::spawn_agent_monitor(Arc::clone(&state), *session_id).await;
    }
    if !restored.is_empty() {
        tracing::info!(
            "Restored {} sessions from previous shutdown",
            restored.len()
        );
    }

    if state.config.server.admin_users.is_empty() {
        tracing::info!("Admin panel disabled (no admin_users configured in beam.toml)");
    }

    let app = web::build_router(Arc::clone(&state))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    let request_id = request
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    tracing::info_span!(
                        "request",
                        method = %request.method(),
                        path = %request.uri().path(),
                        request_id = %request_id,
                    )
                })
                .on_request(|_request: &axum::http::Request<_>, _span: &tracing::Span| {
                    tracing::event!(Level::INFO, "started");
                })
                .on_response(
                    |response: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        tracing::event!(
                            Level::INFO,
                            status = %response.status().as_u16(),
                            duration_ms = %latency.as_millis(),
                            "completed"
                        );
                    },
                ),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    // Clean up old agent logs on startup (keep last 20, remove >24h old)
    cleanup_old_agent_logs(24 * 3600, 20);

    // Print startup banner
    tracing::info!("===========================================");
    tracing::info!(
        "  Beam Remote Desktop Server v{}",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!("  Listening on https://{bind_addr}");
    tracing::info!("===========================================");

    // Bind and serve with TLS
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("Failed to bind to {bind_addr}"))?;

    tracing::info!("Server ready, accepting connections");

    // Background task: reap stale sessions (configurable idle timeout)
    let idle_timeout = state.config.session.idle_timeout;
    if idle_timeout > 0 {
        let reaper_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let stale = reaper_state
                    .session_manager
                    .stale_sessions(idle_timeout)
                    .await;
                for session_id in stale {
                    tracing::info!(%session_id, "Reaping stale session (idle > {idle_timeout}s)");
                    if let Err(e) = reaper_state
                        .session_manager
                        .destroy_session(session_id)
                        .await
                    {
                        tracing::error!(%session_id, "Failed to reap session: {e}");
                    }
                    signaling::remove_channel(&reaper_state.channels, session_id).await;
                }
            }
        });
    } else {
        tracing::info!("Session idle timeout disabled (idle_timeout = 0)");
    }

    // Set up graceful shutdown
    let shutdown_state = Arc::clone(&state);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    // Accept TLS connections and serve with axum
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer_addr) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!("Failed to accept TCP connection: {e}");
                        continue;
                    }
                };

                let acceptor = tls_acceptor.clone();
                let app = app.clone();

                tokio::spawn(async move {
                    // TLS handshake timeout (10 seconds)
                    let tls_stream = match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        acceptor.accept(stream),
                    ).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            tracing::debug!(%peer_addr, "TLS handshake failed: {e}");
                            return;
                        }
                        Err(_) => {
                            tracing::debug!(%peer_addr, "TLS handshake timed out");
                            return;
                        }
                    };

                    // Inject peer address so handlers can extract client IP
                    let app_with_peer = app.layer(axum::Extension(peer_addr));

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let hyper_service = hyper_util::service::TowerToHyperService::new(app_with_peer);
                    let builder = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );

                    if let Err(e) = builder.serve_connection_with_upgrades(io, hyper_service).await {
                        tracing::debug!(%peer_addr, "Connection error: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received SIGINT, initiating graceful shutdown");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, initiating graceful shutdown");
                break;
            }
        }
    }

    // Graceful shutdown: persist sessions so agents survive the restart
    tracing::info!("Persisting sessions for graceful restart...");
    if let Err(e) = shutdown_state.session_manager.persist_sessions().await {
        tracing::error!("Failed to persist sessions, destroying instead: {e}");
        // Fallback: destroy all sessions if persistence fails
        let sessions = shutdown_state.session_manager.list_sessions().await;
        for session in &sessions {
            let _ = shutdown_state
                .session_manager
                .destroy_session(session.id)
                .await;
            signaling::remove_channel(&shutdown_state.channels, session.id).await;
        }
    }

    tracing::info!("Beam server shut down cleanly (sessions persisted)");

    Ok(())
}

/// Remove old agent logs from /var/log/beam/, keeping at most `max_count`
/// and removing any older than `max_age_secs`.
fn cleanup_old_agent_logs(max_age_secs: u64, max_count: usize) {
    let dir = std::path::Path::new("/var/log/beam");
    let _ = std::fs::create_dir_all(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let now = std::time::SystemTime::now();
    let mut logs: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".log"))
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();

    // Sort newest first
    logs.sort_by_key(|b| std::cmp::Reverse(b.1));

    for (i, (path, mtime)) in logs.iter().enumerate() {
        let age = now.duration_since(*mtime).unwrap_or_default().as_secs();
        if i >= max_count || age > max_age_secs {
            let _ = std::fs::remove_file(path);
        }
    }
}
