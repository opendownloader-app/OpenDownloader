//! Letting the browser extension start the bridge itself.
//!
//! # The problem this solves
//!
//! An extension cannot join a BitTorrent swarm: it may not open a socket to a peer, and
//! no permission grants that. Something native has to. Until now that something was this
//! app, which the user had to have *running* — so a magnet worked or did not depending
//! on whether they had opened a program earlier.
//!
//! Native messaging removes that. The browser starts a registered helper on demand, talks
//! to it over stdin and stdout, and stops it when the extension disconnects. Nothing is
//! left running, and nothing is typed.
//!
//! # Why this is a launcher rather than the protocol itself
//!
//! Native messaging is a stdio channel, and the download path is HTTP with byte ranges —
//! that is what makes resume, chunking and verification work, and it is the part that is
//! tested. So the message exchange is one question: *start the bridge and tell me its
//! port*. The bytes then move over HTTP as they always have.
//!
//! # The wire format
//!
//! A four-byte native-endian length, then that many bytes of UTF-8 JSON. Both directions.
//! Anything written to stdout that is not a framed message corrupts the channel, which is
//! why nothing here prints.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

/// What the host is called, in the browser and in the manifest that registers it.
const HOST_NAME: &str = "app.opendownloader.bridge";

/// Read one framed message from stdin, or `None` when the browser has closed the channel.
fn read_message() -> anyhow::Result<Option<serde_json::Value>> {
    let mut length = [0u8; 4];
    let mut stdin = std::io::stdin().lock();
    if stdin.read_exact(&mut length).is_err() {
        return Ok(None);
    }
    // Native endianness, per the protocol — not network order.
    let length = u32::from_ne_bytes(length) as usize;
    // A generous ceiling: these messages are a few dozen bytes, and a wrong length must
    // not become an allocation the size of the machine.
    if length > 64 * 1024 {
        anyhow::bail!("the browser sent an implausible message length");
    }
    let mut buf = vec![0u8; length];
    stdin.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

/// Write one framed message to stdout.
fn write_message(value: &serde_json::Value) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&(body.len() as u32).to_ne_bytes())?;
    stdout.write_all(&body)?;
    stdout.flush()?;
    Ok(())
}

/// Serve the browser: start the bridge, answer with its port, stay alive until dismissed.
///
/// The process lives as long as the extension keeps the port open, which is how the
/// bridge outlives the single message that started it and how it is cleaned up without
/// anyone deciding to.
pub async fn serve() -> anyhow::Result<()> {
    let mut server: Option<tokio::task::JoinHandle<()>> = None;
    let mut port: Option<u16> = None;

    while let Some(message) = read_message()? {
        let kind = message.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "start" => {
                if port.is_none() {
                    match start_bridge().await {
                        Ok((bound, handle)) => {
                            port = Some(bound);
                            server = Some(handle);
                        }
                        Err(e) => {
                            // Reported rather than returned: exiting here reaches the
                            // extension only as "Native host has exited", which names
                            // nothing and reads exactly like the app not being installed.
                            write_message(&json!({
                                "ok": false,
                                "error": format!("the bridge could not start: {e}"),
                            }))?;
                            continue;
                        }
                    }
                }
                write_message(&json!({ "ok": true, "port": port }))?;
            }
            other => {
                write_message(&json!({
                    "ok": false,
                    "error": format!("unknown request {other:?}"),
                }))?;
            }
        }
    }

    // The channel closed: the extension is gone, so the bridge goes with it.
    if let Some(server) = server {
        server.abort();
    }
    Ok(())
}

/// Start the bridge on a port the operating system picks.
///
/// The torrent session takes a lock on its folder, so a second host started while a first
/// one is still alive cannot share it. That happens whenever two pages want a bridge — and
/// the failure was invisible: the process exited during startup and the extension saw only
/// "Native host has exited", which names nothing and reads like the app not being there.
///
/// So the shared folder is tried first, and a folder of this process's own is the
/// fallback. One host is the common case and keeps everything in one place; more than one
/// still works.
async fn start_bridge() -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    let shared = super::torrent_folder();
    std::fs::create_dir_all(&shared)?;

    let bridge = match dl_torrent::router(&shared).await {
        Ok(router) => router,
        Err(_) => {
            let own = shared.join(format!("host-{}", std::process::id()));
            std::fs::create_dir_all(&own)?;
            dl_torrent::router(&own).await?
        }
    };

    let app = axum::Router::new()
        .nest("/relay", super::relay_router()?)
        .nest("/torrent-bridge", bridge);
    let listener =
        tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let port = listener.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((port, handle))
}

/// Where each browser reads native messaging host manifests from.
fn manifest_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let support = home.join("Library/Application Support");
    [
        "Google/Chrome",
        "Google/Chrome Beta",
        "Microsoft Edge",
        "BraveSoftware/Brave-Browser",
        "Chromium",
    ]
    .iter()
    .map(|browser| support.join(browser).join("NativeMessagingHosts"))
    .collect()
}

/// The extension ids allowed to start this host.
///
/// An unpacked extension's id comes from the folder it was loaded from, so a development
/// build and a store build are different ids and both belong here. Listing an id that is
/// not installed costs nothing; omitting one means the extension is refused with a
/// message about the host not being found, which reads like the app is missing.
fn allowed_origins() -> Vec<String> {
    let mut ids = vec![
        // The development build, loaded unpacked from apps/extension/dist.
        "apiifoekhnpccalkflkmodllookaacjh".to_string(),
    ];
    // So a store id can be added without a rebuild.
    if let Ok(extra) = std::env::var("OPENDOWNLOADER_EXTENSION_IDS") {
        ids.extend(
            extra
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    ids.iter()
        .map(|id| format!("chrome-extension://{id}/"))
        .collect()
}

/// Register this binary as the browser's helper, for every Chromium browser installed.
///
/// Writing the manifest is the whole installation: after this the extension can start the
/// bridge itself, and this app need never be running again.
pub fn register() -> anyhow::Result<()> {
    let binary = std::env::current_exe()?;
    let manifest = json!({
        "name": HOST_NAME,
        "description": "OpenDownloader's local bridge: joins BitTorrent swarms and relays requests a page may not make.",
        "path": binary.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": allowed_origins(),
    });
    let body = serde_json::to_vec_pretty(&manifest)?;

    let mut written = 0usize;
    for dir in manifest_dirs() {
        // Only where the browser exists: creating the tree for a browser that is not
        // installed litters the disk with directories nothing will ever read.
        let Some(browser_dir) = dir.parent() else {
            continue;
        };
        if !browser_dir.exists() {
            continue;
        }
        if write_manifest(&dir, &body).is_ok() {
            written += 1;
        }
    }
    if written == 0 {
        anyhow::bail!("no Chromium browser was found to register with");
    }
    Ok(())
}

fn write_manifest(dir: &Path, body: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(format!("{HOST_NAME}.json")), body)?;
    Ok(())
}
