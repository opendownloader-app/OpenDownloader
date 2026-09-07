//! The relay as a program of its own.
//!
//! The desktop build mounts [`dl_relay::router`] inside one process instead; this binary
//! is what the repository checkout runs, and what an operator running a relay for
//! themselves runs.

use dl_relay::config::Config;
use dl_relay::{build_client, cors_layer, router};

#[tokio::main]
async fn main() {
    let cfg = match Config::load(std::env::args().nth(1).as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("dl-relay: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = cfg.parse_bind() {
        eprintln!("dl-relay: {e}");
        std::process::exit(2);
    }

    println!("dl-relay configuration:");
    println!("{cfg}");
    if cfg.allow_private_hosts {
        println!(
            "  ! allow_private_hosts is on: any client of this relay can reach any\n\
             ! host this relay can reach, including everything on its LAN."
        );
    }

    let client = match build_client(&cfg) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("dl-relay: cannot build HTTP client: {e}");
            std::process::exit(2);
        }
    };
    let cors = match cors_layer(&cfg) {
        Ok(layer) => layer,
        Err(e) => {
            eprintln!("dl-relay: {e}");
            std::process::exit(2);
        }
    };

    let bind = cfg.bind.clone();
    let app = router(cfg, client, cors);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("dl-relay: cannot bind {bind}: {e}");
            std::process::exit(2);
        }
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or(bind.clone());
    println!("listening on http://{addr}");

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
    {
        eprintln!("dl-relay: server error: {e}");
        std::process::exit(1);
    }
}

/// Wait for SIGINT, then let `axum` drain.
///
/// In-flight bodies are allowed to finish rather than being cut off: a relay killed
/// mid-download costs the client the whole file, and this is a program an operator will
/// restart often.
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("shutting down");
}
