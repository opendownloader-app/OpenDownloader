//! The BitTorrent bridge: a swarm, served as ordinary HTTP.
//!
//! # Why this exists
//!
//! A `magnet:` link names content by hash. Finding it means asking trackers and the DHT
//! for peers, then opening connections to them over TCP or uTP. A browser tab cannot open
//! those sockets — that is a limit of the tab, not of this machine, and no permission,
//! relay hop or extension API lifts it.
//!
//! So the swarm is joined here instead, in a process that *does* have sockets, and the
//! result is offered back over HTTP with byte ranges. That last part is the whole point:
//! once a file in a torrent answers a `Range` request, it is an ordinary download, and
//! everything the downloader already does — parallel chunks, resume after an interruption,
//! the SHA-256 read back off disk at the end — works against it unchanged. No new
//! download path, no second progress model, no torrent-shaped special case in the UI.
//!
//! # What it is not
//!
//! It is not a torrent *client*. It does not seed after a download finishes, keep a
//! library, or run when nothing has asked it to. It is the piece of plumbing that a
//! browser cannot be, and nothing more.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use librqbit::{AddTorrent, AddTorrentOptions, Session};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};

/// Where finished and in-progress pieces are written.
///
/// A torrent is not streamed straight through: pieces arrive out of order and have to
/// land somewhere before a byte range over them can be answered. This is that somewhere.
fn default_output_folder() -> PathBuf {
    std::env::temp_dir().join("opendownloader-torrents")
}

#[derive(Clone)]
struct Bridge {
    session: Arc<Session>,
    /// Kept alongside the session because librqbit does not expose its own copy, and
    /// `/healthz` says where the bytes land so a person can find and delete them.
    output_folder: PathBuf,
}

/// One file inside a torrent, as the browser needs to see it.
#[derive(Serialize)]
struct FileEntry {
    index: usize,
    name: String,
    length: u64,
    /// The URL that serves this file's bytes. Absolute-path form, so the page can join it
    /// to whichever address it reached this bridge on.
    url: String,
}

#[derive(Serialize)]
struct TorrentAdded {
    id: usize,
    name: String,
    info_hash: String,
    files: Vec<FileEntry>,
}

#[derive(Serialize)]
struct Health {
    service: &'static str,
    version: &'static str,
    output_folder: String,
}

/// A refusal, in the same shape the relay uses: a status and a sentence for a person.
struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(why: impl Into<String>) -> Failure {
    Failure(StatusCode::BAD_REQUEST, why.into())
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

    let session = Session::new(output_folder.clone()).await?;
    let bridge = Bridge {
        session,
        output_folder: output_folder.clone(),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/torrent", post(add_torrent))
        .route("/torrent/{id}", get(torrent_status).delete(forget_torrent))
        .route("/torrent/{id}/file/{index}", get(serve_file))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                // `range` so a resumed or chunked download can ask for one, and the
                // exposed trio so the page can read what came back.
                .allow_headers([header::RANGE, header::CONTENT_TYPE])
                .expose_headers([
                    header::CONTENT_LENGTH,
                    header::CONTENT_RANGE,
                    header::ACCEPT_RANGES,
                    header::CONTENT_DISPOSITION,
                ]),
        )
        .with_state(bridge);

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

async fn healthz(State(bridge): State<Bridge>) -> Json<Health> {
    Json(Health {
        service: "dl-torrent",
        version: env!("CARGO_PKG_VERSION"),
        output_folder: bridge.output_folder.display().to_string(),
    })
}

/// Add a magnet link, an `http(s)` link to a `.torrent`, or the bytes of one.
///
/// The body is the link as text, or the `.torrent` itself. Which of the two is decided by
/// looking at the bytes rather than by trusting `Content-Type`, because a file input and a
/// pasted string reach here labelled inconsistently across browsers.
async fn add_torrent(
    State(bridge): State<Bridge>,
    body: axum::body::Bytes,
) -> Result<Json<TorrentAdded>, Failure> {
    if body.is_empty() {
        return Err(bad_request(
            "send a magnet link, or the bytes of a .torrent",
        ));
    }

    // A bencoded .torrent always begins with `d`; a link never does.
    let add = match std::str::from_utf8(&body) {
        Ok(text)
            if text.trim_start().starts_with("magnet:")
                || text.trim_start().starts_with("http://")
                || text.trim_start().starts_with("https://") =>
        {
            AddTorrent::Url(text.trim().to_string().into())
        }
        _ => AddTorrent::TorrentFileBytes(body),
    };

    let response = bridge
        .session
        .add_torrent(
            add,
            Some(AddTorrentOptions {
                // Nothing is fetched until a file is actually asked for: adding a torrent
                // is how the user finds out what is *in* it, and downloading all of it to
                // answer that question is exactly the behaviour that makes torrent clients
                // eat a disk.
                paused: true,
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| bad_request(format!("that torrent could not be added: {e:#}")))?;

    let handle = response
        .into_handle()
        .ok_or_else(|| bad_request("that torrent has no files to offer"))?;

    // A magnet carries only a hash; the file list arrives from a peer afterwards. Waiting
    // is the honest thing to do here — the alternative is answering with an empty list
    // and letting the page poll, which is the same wait moved somewhere less visible.
    tokio::time::timeout(Duration::from_secs(90), handle.wait_until_initialized())
        .await
        .map_err(|_| {
            Failure(
                StatusCode::GATEWAY_TIMEOUT,
                "no peer answered with this torrent's file list within 90 seconds. The \
                 swarm may be empty, or the network may be blocking peer connections."
                    .into(),
            )
        })?
        .map_err(|e| bad_request(format!("that torrent could not be read: {e:#}")))?;

    let id = handle.id();
    let files = handle
        .with_metadata(|m| {
            m.file_infos
                .iter()
                .enumerate()
                .map(|(index, f)| FileEntry {
                    index,
                    name: f.relative_filename.to_string_lossy().to_string(),
                    length: f.len,
                    url: format!("/torrent/{id}/file/{index}"),
                })
                .collect::<Vec<_>>()
        })
        .map_err(|e| bad_request(format!("that torrent's file list is unreadable: {e:#}")))?;

    Ok(Json(TorrentAdded {
        id,
        name: handle.name().unwrap_or_else(|| format!("torrent-{id}")),
        info_hash: format!("{:?}", handle.info_hash()),
        files,
    }))
}

async fn torrent_status(
    State(bridge): State<Bridge>,
    AxumPath(id): AxumPath<usize>,
) -> Result<Json<serde_json::Value>, Failure> {
    let handle = bridge
        .session
        .get(id.into())
        .ok_or_else(|| Failure(StatusCode::NOT_FOUND, "no such torrent".into()))?;
    let stats = handle.stats();
    // Peers, because without them "Live, 0 bytes" is indistinguishable from a bug in
    // this bridge. A magnet resolves its metadata from peers that hold only that, so a
    // torrent can name its files in seconds and then never transfer a byte — which is
    // what a swarm with no seeders looks like from here. Saying how many peers are
    // connected turns a hang into a fact the reader can act on.
    let peers = stats.live.as_ref().map(|live| {
        let p = &live.snapshot.peer_stats;
        serde_json::json!({
            "live": p.live,
            "connecting": p.connecting,
            "queued": p.queued,
            "seen": p.seen,
            "dead": p.dead,
            "download_bytes_per_second": live.download_speed.mbps * 125_000.0,
        })
    });
    Ok(Json(serde_json::json!({
        "id": id,
        "name": handle.name(),
        "state": format!("{:?}", stats.state),
        "progress_bytes": stats.progress_bytes,
        "total_bytes": stats.total_bytes,
        "finished": stats.finished,
        "peers": peers,
    })))
}

/// Drop a torrent and delete what it wrote.
///
/// Called when the page is done with it. Without this the temp folder grows by the size
/// of every torrent ever opened, including the ones opened to look at the file list and
/// then abandoned.
async fn forget_torrent(
    State(bridge): State<Bridge>,
    AxumPath(id): AxumPath<usize>,
) -> Result<StatusCode, Failure> {
    bridge
        .session
        .delete(id.into(), true)
        .await
        .map_err(|e| bad_request(format!("could not forget that torrent: {e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Serve one file's bytes, honouring `Range`.
///
/// This is the whole bridge. `FileStream` is `AsyncRead + AsyncSeek` over the torrent, and
/// seeking it tells librqbit which pieces matter — so a range request pulls the pieces
/// covering that range rather than the whole torrent, and a download of one file out of a
/// season pack does not fetch the season.
async fn serve_file(
    State(bridge): State<Bridge>,
    AxumPath((id, index)): AxumPath<(usize, usize)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let handle = bridge
        .session
        .get(id.into())
        .ok_or_else(|| Failure(StatusCode::NOT_FOUND, "no such torrent".into()))?;

    // Added paused so that adding one costs nothing; the first request for actual bytes
    // is what starts it. Unpausing an already-running torrent is a no-op, so this needs
    // no "have I started it yet" bookkeeping of its own.
    if handle.is_paused() {
        bridge
            .session
            .unpause(&handle)
            .await
            .map_err(|e| bad_request(format!("that torrent could not be started: {e:#}")))?;
    }

    let mut stream = handle
        .clone()
        .stream(index)
        .await
        .map_err(|e| bad_request(format!("that file cannot be read: {e:#}")))?;
    let total = stream.len();

    let filename = handle
        .with_metadata(|m| {
            m.file_infos
                .get(index)
                .map(|f| f.relative_filename.to_string_lossy().to_string())
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| format!("file-{index}"));

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total));

    let (status, start, length) = match range {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        None => (StatusCode::OK, 0, total),
    };

    if start > 0 {
        stream
            .seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| bad_request(format!("could not seek that file: {e}")))?;
    }

    let body = Body::from_stream(ReaderStream::new(stream.take(length)));
    let mut response = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length)
        .header(header::CONTENT_TYPE, guess_mime(&filename))
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename*=UTF-8''{}", urlencode(&filename)),
        );
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, start + length - 1, total),
        );
    }
    response
        .body(body)
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Parse a single-range `bytes=` header into an inclusive `(start, end)`.
///
/// Only one range: a multipart range response is a different document format, and nothing
/// in this product asks for one. Anything unparseable returns `None`, which serves the
/// whole file — the same thing every well-behaved server does with a range it cannot
/// satisfy in the form asked.
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?.trim();
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        // `bytes=-500`: the last 500 bytes.
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            (total.saturating_sub(n.min(total)), total - 1)
        }
        // `bytes=500-`: from 500 to the end.
        (from, "") => (from.parse().ok()?, total - 1),
        (from, to) => (from.parse().ok()?, to.parse::<u64>().ok()?.min(total - 1)),
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

/// Enough of a type map to keep a browser from guessing wrong on the common cases.
fn guess_mime(name: &str) -> HeaderValue {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "srt" => "application/x-subrip",
        "ass" | "ssa" => "text/x-ssa",
        "vtt" => "text/vtt",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(mime)
}

/// Percent-encode for `filename*=UTF-8''…`, which is how a non-ASCII name survives the
/// trip. Torrent names are routinely CJK, and an unencoded one truncates the header.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_range_is_inclusive_at_both_ends() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-199", 1000), Some((100, 199)));
    }

    #[test]
    fn an_open_ended_range_runs_to_the_last_byte() {
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
    }

    #[test]
    fn a_suffix_range_counts_back_from_the_end() {
        assert_eq!(parse_range("bytes=-200", 1000), Some((800, 999)));
        // Asking for more than there is yields the whole file, not an error.
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn an_end_past_the_file_is_clamped_rather_than_refused() {
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)));
    }

    #[test]
    fn ranges_this_cannot_answer_fall_back_to_the_whole_file() {
        assert_eq!(parse_range("bytes=0-99,200-299", 1000), None);
        assert_eq!(parse_range("items=0-99", 1000), None);
        assert_eq!(parse_range("bytes=abc-def", 1000), None);
        // Start beyond the end is not a range this can serve.
        assert_eq!(parse_range("bytes=1000-1099", 1000), None);
        assert_eq!(parse_range("bytes=99-0", 1000), None);
        // A zero-length file has no byte to point at.
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn a_cjk_filename_survives_the_content_disposition_header() {
        let encoded = urlencode("群体.1080p.mp4");
        assert!(encoded.starts_with("%E7%BE%A4%E4%BD%93"));
        assert!(encoded.ends_with(".1080p.mp4"));
        assert!(HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{encoded}")).is_ok());
    }

    #[test]
    fn video_extensions_are_typed_rather_than_left_as_octet_stream() {
        assert_eq!(guess_mime("a.mkv"), "video/x-matroska");
        assert_eq!(guess_mime("群体.MP4"), "video/mp4");
        assert_eq!(guess_mime("readme.nfo"), "application/octet-stream");
    }
}
