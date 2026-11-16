use std::path::PathBuf;
use std::sync::Arc;

use gally_uring::{backup, router, state::AppState};

/// Same resolution as the server: `GALLY_ROOT`, else the working directory.
/// Not the compile-time crate dir: a copied binary must not look for its data
/// on the build machine.
fn root() -> PathBuf {
    std::env::var("GALLY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--backup") {
        let Some(dest) = args.get(1) else {
            eprintln!("usage: gally-uring --backup <destination-dir>");
            std::process::exit(2);
        };
        // Backup runs before any state is built: no locks, no server.
        return backup::run(&root(), &PathBuf::from(dest));
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gally_uring=info")),
        )
        .init();
    let state: Arc<AppState> = AppState::new(&root()).await?;
    let app = router(state);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8000);
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// SIGINT (Ctrl+C) or SIGTERM (Docker stop): stop accepting, drain in-flight
/// requests, then let the instance lock drop with the state. compose.yaml
/// allows 120 s for this.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
