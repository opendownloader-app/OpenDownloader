//! The torrent bridge as a program of its own.
//!
//! The desktop build mounts [`dl_torrent::router`] inside one process instead; this
//! binary is what the repository checkout runs, and what anyone wanting the bridge
//! without the rest can run.

use std::net::SocketAddr;
use std::path::PathBuf;

fn default_output_folder() -> PathBuf {
    std::env::temp_dir().join("opendownloader-torrents")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let output_folder = std::env::var("DL_TORRENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_output_folder());
    std::fs::create_dir_all(&output_folder)?;

    let port: u16 = std::env::var("DL_TORRENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8089);
    // Loopback only, and deliberately not configurable to anything else. This process
    // joins swarms on behalf of whoever can reach it; that must be this machine.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let app = dl_torrent::router(&output_folder).await?;

    println!("dl-torrent");
    println!("  listening on  http://{addr}");
    println!("  writing to    {}", output_folder.display());
    println!();
    println!("  This process joins BitTorrent swarms on behalf of the page. It is bound to");
    println!("  loopback and does nothing until a torrent is added.");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
