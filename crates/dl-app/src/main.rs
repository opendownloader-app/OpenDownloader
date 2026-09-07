//! OpenDownloader as one program you download and open.
//!
//! # Why this exists
//!
//! The web app needs two helpers to do its whole job — a relay for the sites that refuse
//! a browser's headers, and a bridge for BitTorrent, which needs sockets no page can
//! open. Both must run on the user's machine: hosting them would make us a proxy for
//! other people's downloads, on our IP and in the middle of traffic we have no reason to
//! see.
//!
//! Until now "on the user's machine" meant cloning a repository and running `npm start`,
//! which is a developer's workflow wearing a product's clothes. This is the same three
//! things in one binary: double-click it and the browser opens on a working app.
//!
//! # One port, on purpose
//!
//! Everything is served from a single loopback origin rather than three. That is not
//! tidiness — it removes three separate browser rules that each broke this before:
//!
//! - **Mixed content.** A page on `https://app.opendownloader.app` may not talk to
//!   `http://127.0.0.1`, so the hosted site could never reach a local helper however it
//!   was installed. Here the page *is* local, so there is no mix.
//! - **CORS.** Same origin, so no preflight and nothing to allow.
//! - **Private Network Access.** No public page reaching into a private address, because
//!   the page is already there.
//!
//! The web app is served at `/`, the relay under `/relay`, the bridge under `/torrent`.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use rust_embed::RustEmbed;

mod native_host;

/// The built web app, baked into the binary so there is one file to ship.
#[derive(RustEmbed)]
#[folder = "../../apps/web/dist"]
struct WebApp;

fn torrent_folder() -> PathBuf {
    std::env::temp_dir().join("opendownloader-torrents")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Chrome starts a native messaging host with the calling extension's origin as an
    // argument, and talks to it over stdin and stdout. Nothing may be printed in that
    // mode — stdout *is* the protocol — and no browser window is opened, because the
    // browser is already there and asking.
    if std::env::args().any(|a| a.starts_with("chrome-extension://")) {
        return native_host::serve().await;
    }

    let folder = torrent_folder();
    std::fs::create_dir_all(&folder)?;

    let app = Router::new()
        .nest("/relay", relay_router()?)
        .nest("/torrent-bridge", dl_torrent::router(&folder).await?)
        .fallback(serve_web_app);

    let listener = match listen().await {
        Ok(listener) => listener,
        Err(why) => {
            // Launched from Finder there is no terminal to print to, so a failure here
            // is a program that appears to do nothing at all. Say it where it can be
            // seen.
            report(&why);
            anyhow::bail!(why);
        }
    };
    let addr = listener.local_addr()?;
    // Registering here rather than in an installer means opening the app once is the
    // entire setup: from then on the extension can start the bridge by itself, and the
    // app does not have to be running at all.
    if let Err(e) = native_host::register() {
        eprintln!("could not register the browser helper: {e}");
    }

    // Advertised for the same reason the browser-started host does it: whichever of the
    // two is running first, the other must find it rather than start a second torrent
    // session, which cannot bind the DHT socket a second time.
    native_host::advertise(&format!("http://{addr}/torrent-bridge"));

    let url = format!("http://{addr}/");
    println!("OpenDownloader is running at {url}");
    println!("Close this window to stop it.");
    open_browser(&url);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Bind the first free loopback port, preferring the usual one.
///
/// A fixed port is the wrong shape for something people download and open: anything else
/// already listening — another copy, a dev server left running, an unrelated program —
/// makes the app die at launch with no window and no message. The browser is opened with
/// whatever port was taken, so the number never has to be remembered.
///
/// Loopback only, and deliberately not configurable to anything else: this process
/// fetches on behalf of whoever can reach it and joins swarms for them, and that must be
/// this machine.
async fn listen() -> Result<tokio::net::TcpListener, String> {
    let preferred: u16 = std::env::var("OPENDOWNLOADER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5180);

    let mut last = String::new();
    for port in (preferred..preferred.saturating_add(12)).chain(std::iter::once(0)) {
        // Port 0 last: the operating system picks a free one, so this only fails when
        // there is no loopback to bind at all.
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    Err(format!(
        "OpenDownloader could not open a port to run on. Last try was {last}."
    ))
}

/// Put a message somewhere a person will see it, however the app was started.
fn report(message: &str) {
    eprintln!("{message}");
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog {:?} with title \"OpenDownloader\" buttons {{\"OK\"}} default button 1 with icon caution",
            message
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    }
}

/// The relay, configured for local use.
fn relay_router() -> anyhow::Result<Router> {
    let cfg = dl_relay::config::Config::default();
    let client = dl_relay::build_client(&cfg)?;
    let cors = dl_relay::cors_layer(&cfg)
        .map_err(|e| anyhow::anyhow!("the relay's CORS settings are unusable: {e}"))?;
    Ok(dl_relay::router(cfg, client, cors))
}

/// Serve a file from the embedded web app, falling back to `index.html`.
///
/// The fallback is what makes a deep link work: the app routes `/account` itself, and a
/// reload of that path must not 404.
async fn serve_web_app(uri: axum::http::Uri) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let file = WebApp::get(path).or_else(|| WebApp::get("index.html"));
    match file {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Open the default browser, and say what to do if it does not.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let opened = std::process::Command::new("open").arg(url).spawn().is_ok();
    #[cfg(target_os = "windows")]
    let opened = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .is_ok();
    #[cfg(all(unix, not(target_os = "macos")))]
    let opened = std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .is_ok();

    if !opened {
        println!("Open this in your browser: {url}");
    }
}
