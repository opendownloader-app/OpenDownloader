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
    let mut url: Option<String> = None;

    while let Some(message) = read_message()? {
        let kind = message.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "start" => {
                if url.is_none() {
                    match start_bridge().await {
                        Ok((found, handle)) => {
                            url = Some(found);
                            server = handle;
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
                write_message(&json!({ "ok": true, "url": url }))?;
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
async fn start_bridge() -> anyhow::Result<(String, Option<tokio::task::JoinHandle<()>>)> {
    // One already running is the answer where there is one. A torrent session takes a
    // lock on its folder *and* binds a DHT socket, and neither can be held twice — so a
    // second session started while the app is open fails with "error initializing
    // persistent DHT", which says nothing to anyone. Reusing what is there avoids the
    // collision entirely and is what a user means by "the bridge".
    if let Some(existing) = find_running_bridge().await {
        return Ok((existing, None));
    }

    let folder = super::torrent_folder();
    std::fs::create_dir_all(&folder)?;
    let bridge = dl_torrent::router(&folder).await?;

    let app = axum::Router::new()
        .nest("/relay", super::relay_router()?)
        .nest("/torrent-bridge", bridge);
    let listener =
        tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let port = listener.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((
        format!("http://127.0.0.1:{port}/torrent-bridge"),
        Some(handle),
    ))
}

/// Where a running bridge writes the address it can be reached at.
///
/// A browser-started host binds a port the operating system picks, so nothing else can
/// guess it — and guessing wrongly is not harmless: a second torrent session cannot
/// start while a first is alive, because the DHT socket is already bound, and the
/// collision surfaces as "error initializing persistent DHT". Leaving the address behind
/// is what lets the next host find the first instead of colliding with it.
fn advert_path() -> PathBuf {
    std::env::temp_dir().join("opendownloader-bridge.json")
}

/// Record where this bridge is, for the next host that looks.
pub fn advertise(url: &str) {
    let _ = std::fs::write(
        advert_path(),
        serde_json::to_vec(&json!({ "url": url, "pid": std::process::id() })).unwrap_or_default(),
    );
}

/// A bridge already listening on this machine, if there is one.
///
/// The advert first, since a browser-started host is on a port nobody could guess. Then
/// the two fixed shapes: the app serves everything on one port and mounts the bridge
/// under a path, walking up from 5180 when that port is taken; the standalone binary sits
/// on 8089 with no prefix.
///
/// Every candidate is health-checked rather than trusted — an advert outlives the process
/// that wrote it, and these ports are ordinary ones that anything could be sitting on.
async fn find_running_bridge() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(400))
        .build()
        .ok()?;

    let mut candidates = Vec::new();
    if let Ok(text) = std::fs::read_to_string(advert_path()) {
        if let Ok(advert) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(url) = advert.get("url").and_then(|v| v.as_str()) {
                candidates.push(url.to_string());
            }
        }
    }
    candidates.push("http://127.0.0.1:8089".to_string());
    candidates.extend((5180..5192).map(|p| format!("http://127.0.0.1:{p}/torrent-bridge")));

    for base in candidates {
        let Ok(response) = client.get(format!("{base}/healthz")).send().await else {
            continue;
        };
        let Ok(body) = response.text().await else {
            continue;
        };
        if body.contains("\"dl-torrent\"") {
            return Some(base);
        }
    }
    None
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
/// Discovered rather than hardcoded, because an unpacked extension's id is derived from
/// the folder it was loaded from — so it differs between one machine and the next, and
/// between a development copy and a store build. A host registered for the wrong id
/// refuses the extension with "Access to the specified native messaging host is
/// forbidden", which reads like the app being broken rather than a name mismatch.
///
/// So every Chromium profile is asked which extensions it has, and any whose path or name
/// says OpenDownloader is allowed. A known id and an environment override are added on
/// top, for a store build that is not installed here yet.
fn allowed_origins() -> Vec<String> {
    let mut ids: Vec<String> = installed_extension_ids();

    // The extension's permanent id, fixed by the `key` field in its manifest.
    //
    // Without that key an id is derived from the folder the extension was loaded from —
    // so it differed per machine, and changed whenever the folder moved. The host allows
    // one id, so it was wrong again each time, and the browser refuses a mismatch with
    // "Access to the specified native messaging host is forbidden". Worse, browsers cache
    // this manifest, so correcting it did not take effect until the browser restarted.
    //
    // A fixed id makes registration a thing that happens once and stays true.
    ids.push("cbeecjhjblfbdjncacbfmlcelgkmklda".to_string());
    // Ids of anything installed that looks like ours, for a build that predates the key.
    ids.push("apiifoekhnpccalkflkmodllookaacjh".to_string());
    if let Ok(extra) = std::env::var("OPENDOWNLOADER_EXTENSION_IDS") {
        ids.extend(
            extra
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }

    ids.sort();
    ids.dedup();
    ids.iter()
        .map(|id| format!("chrome-extension://{id}/"))
        .collect()
}

/// Extension ids that look like ours, from every Chromium profile on this machine.
fn installed_extension_ids() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let support = home.join("Library/Application Support");
    let mut found = Vec::new();

    for browser in [
        "Google/Chrome",
        "Google/Chrome Beta",
        "Microsoft Edge",
        "BraveSoftware/Brave-Browser",
        "Chromium",
    ] {
        let Ok(profiles) = std::fs::read_dir(support.join(browser)) else {
            continue;
        };
        for profile in profiles.filter_map(Result::ok) {
            // Both files are read: an unpacked extension is usually in `Secure
            // Preferences`, and which one holds it is not worth predicting.
            for name in ["Secure Preferences", "Preferences"] {
                let path = profile.path().join(name);
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(settings) = json
                    .pointer("/extensions/settings")
                    .and_then(|v| v.as_object())
                else {
                    continue;
                };
                for (id, entry) in settings {
                    // The path is the reliable marker: an unpacked extension's cached
                    // manifest name is often empty, as it is on the machine this was
                    // written for.
                    let where_from = entry
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let called = entry
                        .pointer("/manifest/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if where_from.contains("opendownloader") || called.contains("opendownloader") {
                        found.push(id.clone());
                    }
                }
            }
        }
    }
    found
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
