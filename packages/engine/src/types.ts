// Types shared by the engine, the extension pages and the web app.
//
// The candidate and playlist shapes mirror `dl_core`'s serde output exactly —
// they are what `classify_request` and `parse_playlist_js` return as JSON, so
// any change here needs a matching change in crates/dl-core/src/{classify,hls}.rs.

/** What `dl_core::classify` decided an observed response was. */
export type MediaKind = "progressive" | "hlsplaylist";

/**
 * What the engine runs for a job.
 *
 * A superset of [`MediaKind`], because one job type has no classifier equivalent:
 * `merge` fetches a video stream and an audio stream chosen from a site extractor and
 * combines them into one file. The platforms that need it — YouTube above 360p,
 * Bilibili's DASH — no longer offer a single file to classify.
 */
export type JobKind = MediaKind | "merge";

export interface MediaCandidate {
  url: string;
  kind: MediaKind;
  filename: string;
  mime: string | null;
  size: number | null;
}

/** A candidate plus where it was seen. */
export interface DetectedItem extends MediaCandidate {
  tabId: number;
  pageUrl: string;
  detectedAt: number;
}

export type JobStatus =
  | "queued"
  | "probing"
  | "downloading"
  | "verifying"
  | "done"
  | "paused"
  | "error";

/**
 * How the finished file compares to a digest the user supplied.
 *
 * `unverified` means no expected digest was given: the read-back SHA-256 is
 * still computed and shown, there is just nothing to compare it against.
 */
export type Verification = "unverified" | "verified" | "mismatch";

/**
 * Where a job's bytes are going.
 *
 * `file` is a real seekable handle on disk — whether it was chosen in a save
 * dialog or created inside the download folder does not change anything the
 * engine does with it, so it is not a third case.
 */
export type SinkKind = "file" | "blob";

export interface Job {
  id: string;
  url: string;
  filename: string;
  kind: JobKind;
  status: JobStatus;
  /** Persisted `SessionState` JSON from the Rust side. */
  stateJson: string;
  totalBytes: number | null;
  receivedBytes: number;
  outputBytes: number;
  /** Populated once the read-back verification pass completes. */
  sha256: string | null;
  error: string | null;
  createdAt: number;
  /** Queue position; lower runs first. Defaults to `createdAt` for old records. */
  order: number;
  /** The page the media was found on, for display only. */
  pageUrl?: string;
  /** A digest the user expects the finished file to have. Lowercase hex. */
  expectedSha256?: string | null;
  /**
   * An eD2k hash to check the finished file against, from an `ed2k://` link.
   *
   * Separate from `expectedSha256` rather than a generic "expected digest" pair because
   * the two are checked at once: an ed2k link states its hash, and the read-back still
   * produces the SHA-256 every other job is described by.
   */
  expectedEd2k?: string | null;
  /** The eD2k hash of the finished file, computed only when one was expected. */
  ed2k?: string | null;
  verification?: Verification;
  /** Which sink the job is being written through, once one has been opened. */
  sinkKind?: SinkKind;
  /** HLS only: the resolved segment list, so a resumed job needs no refetch. */
  segments?: HlsSegment[];
  /** HLS only: an `#EXT-X-MAP` init segment, appended before any media segment. */
  initSegment?: HlsSegment | null;
  /** HLS only: whether segments need TS→fMP4 remuxing, or are already fMP4. */
  remux?: boolean;
  /** HLS only: the rendition the user picked. Unset means highest bandwidth. */
  variantUrl?: string;
  /** HLS only: drop the video track and write an audio file. */
  audioOnly?: boolean;
  /** HLS only: an alternate audio playlist to download instead of the variant. */
  audioRenditionUrl?: string | null;
  /** HLS only: subtitle renditions discovered in the master playlist. */
  subtitles?: Rendition[];
  /** HLS only: alternate audio renditions discovered in the master playlist. */
  audioRenditions?: Rendition[];
  /**
   * `merge` only: the two streams to fetch and combine.
   *
   * Stored on the job rather than re-extracted at run time, because a site extractor's
   * URLs are signed and time-limited — re-running the extractor later would produce
   * different URLs, and a job that had already written bytes would splice two different
   * encodings together.
   */
  mergeStreams?: [ExtractedStream, ExtractedStream];
  /** Where a site-extracted job came from, for display. */
  site?: string;
  /**
   * The rendition the user picked, for display.
   *
   * Two jobs for the same video differ only in this, so a queue without it is a list of
   * identical-looking rows.
   */
  quality?: string;
  /**
   * Headers this job's fetches must carry, from the site extractor.
   *
   * Almost always a `Referer`, and almost always the difference between 200 and 403 on
   * a platform CDN. `fetch` refuses to set `Referer` — the Fetch standard lists it as a
   * forbidden header name — so the host applies these some other way, and in the
   * extension that means a temporary `declarativeNetRequest` rule. A host that cannot
   * apply them says so rather than sending the request bare and reporting a 403.
   */
  requestHeaders?: [string, string][];
  /**
   * The largest byte range this host will serve in one request.
   *
   * A hard constraint from the site extractor, not a preference. Google's media hosts
   * answer 403 to any range above 1 MiB, so an engine using its ordinary 8 MiB chunk
   * fails every YouTube download with a status that reads like an authorisation problem.
   */
  maxChunkBytes?: number;
}

export interface HlsSegment {
  url: string;
  byte_range: { start: number; end: number } | null;
  duration_ms: number;
}

export interface HlsVariant {
  url: string;
  bandwidth: number;
  resolution: [number, number] | null;
  codecs: string | null;
  audio_group: string | null;
  subtitles_group: string | null;
}

/** An `#EXT-X-MEDIA` entry: alternate audio or a subtitle track. */
export interface Rendition {
  group_id: string;
  name: string;
  language: string | null;
  /** Absent when the rendition is muxed into the variant itself. */
  url: string | null;
  default: boolean;
  autoselect: boolean;
  forced: boolean;
}

export interface MasterPlaylist {
  variants: HlsVariant[];
  audio: Rendition[];
  subtitles: Rendition[];
}

export interface MediaPlaylist {
  init: HlsSegment | null;
  segments: HlsSegment[];
  total_duration_ms: number;
  is_live: boolean;
}

/** The `Playlist` enum as serde serializes it. */
export type ParsedPlaylist =
  { Master: MasterPlaylist } | { Media: MediaPlaylist };

/** One subtitle cue, as `dl_core::subs::Cue` serializes it. */
export interface Cue {
  start_ms: number;
  end_ms: number;
  text: string;
  settings: string | null;
}

/** One stream of an extracted option. Mirrors `dl_core::sites::Stream`. */
export interface ExtractedStream {
  url: string;
  kind: "muxed" | "videoonly" | "audioonly";
  mime: string | null;
  size: number | null;
  headers: [string, string][];
  /** See `Job.maxChunkBytes`. Null when the host states no limit. */
  max_chunk: number | null;
}

/** Live progress for a running job. Kept in memory, never persisted. */
export interface Progress {
  received: number;
  total: number | null;
  status: JobStatus;
  message?: string;
  bytesPerSecond?: number;
  etaSeconds?: number | null;
}
