//! One download job, from the engine's point of view.
//!
//! [`DownloadSession`] is the object the manager tab holds for the life of a download.
//! It owns the resume state, the integrity hasher and — for HLS jobs — the remuxer, and
//! it is the only thing that needs to survive a page reload: everything about a job can
//! be reconstructed from [`SessionState`], which is plain JSON.
//!
//! This lives here rather than in `wasm.rs` so it can be tested natively. `wasm.rs` is a
//! thin `#[wasm_bindgen]` shell over exactly this type.

use dl_container::{RemuxError, RemuxState, Remuxer};
use serde::{Deserialize, Serialize};

use crate::integrity::Hasher;
use crate::plan::{plan_chunks, ByteRange, ResumeState};

/// The persistable half of a session — everything needed to resume after the manager
/// tab is closed and reopened, or the browser is restarted.
///
/// Excludes the hasher, which does not need persisting: verification happens in one
/// pass over the finished file rather than incrementally during the download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub resume: ResumeState,
    /// Whether output passes through the TS → fMP4 remuxer.
    pub remux: bool,
    /// The remuxer's carry-over, so an interrupted HLS job continues the same output
    /// file rather than starting a second one on top of it.
    #[serde(default)]
    pub remux_state: Option<RemuxState>,
    /// Index of the next HLS segment to fetch. Ignored for progressive downloads.
    #[serde(default)]
    pub next_segment: usize,
    /// Bytes written to the sink so far. For a remuxed job this differs from
    /// `resume.downloaded()`, because the bytes written are the muxed output, not the
    /// segments that were fetched.
    #[serde(default)]
    pub output_len: u64,
}

impl SessionState {
    pub fn new(total: Option<u64>, accepts_ranges: bool, remux: bool) -> Self {
        Self {
            resume: ResumeState::new(total, accepts_ranges),
            remux,
            remux_state: None,
            next_segment: 0,
            output_len: 0,
        }
    }

    pub fn to_json(&self) -> String {
        // Serializing a plain struct of primitives and Vecs cannot fail.
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}

/// A live download.
pub struct DownloadSession {
    state: SessionState,
    hasher: Hasher,
    /// A second digest over the same read-back, computed only when something is going to
    /// check it. eD2k links carry their own hash and nothing else does, so this stays
    /// `None` for every ordinary download rather than costing an MD4 pass per file.
    ed2k: Option<crate::ed2k::Ed2kHasher>,
    remuxer: Option<Remuxer>,
}

impl DownloadSession {
    /// `audio_only` drops the video track, producing an audio file from a stream
    /// that carries both. It only has meaning when `remux` is set; a passthrough
    /// job has no demuxed tracks to choose between.
    pub fn new(total: Option<u64>, accepts_ranges: bool, remux: bool, audio_only: bool) -> Self {
        Self {
            state: SessionState::new(total, accepts_ranges, remux),
            hasher: Hasher::new(),
            ed2k: None,
            remuxer: remux.then(|| {
                if audio_only {
                    Remuxer::audio_only()
                } else {
                    Remuxer::new()
                }
            }),
        }
    }

    /// Rebuild a session from persisted state.
    ///
    /// A remuxing job restores its [`Remuxer`] from the persisted carry-over, so it
    /// continues the same output file — same fragment sequence, same decode-time
    /// origin, no second init segment. Resume granularity is one HLS segment, which is
    /// also the smallest independently decodable unit, so nothing finer would be safe.
    pub fn restore(json: &str) -> Result<Self, String> {
        let state = SessionState::from_json(json)?;
        let remuxer = state.remux.then(|| match &state.remux_state {
            Some(rs) => Remuxer::restore(rs),
            None => Remuxer::new(),
        });
        Ok(Self {
            state,
            hasher: Hasher::new(),
            ed2k: None,
            remuxer,
        })
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Snapshot for persisting. The remuxer's carry-over is captured here rather than
    /// mirrored into `state` on every segment, so there is exactly one source of truth
    /// for it and no way for the two to drift apart.
    pub fn state_json(&self) -> String {
        let mut snapshot = self.state.clone();
        snapshot.remux_state = self.remuxer.as_ref().map(Remuxer::state);
        snapshot.to_json()
    }

    pub fn set_validator(&mut self, validator: Option<String>) {
        self.state.resume.validator = validator;
    }

    pub fn validator(&self) -> Option<&str> {
        self.state.resume.validator.as_deref()
    }

    pub fn plan(&self, chunk_size: u64, max_parallel: usize) -> Vec<ByteRange> {
        plan_chunks(&self.state.resume, chunk_size, max_parallel)
    }

    pub fn record(&mut self, start: u64, end: u64) {
        self.state.resume.record(ByteRange { start, end });
    }

    pub fn note_output(&mut self, len: u64) {
        self.state.output_len += len;
    }

    pub fn next_segment(&self) -> usize {
        self.state.next_segment
    }

    pub fn downloaded(&self) -> u64 {
        self.state.resume.downloaded()
    }

    pub fn output_len(&self) -> u64 {
        self.state.output_len
    }

    pub fn is_complete(&self) -> bool {
        self.state.resume.is_complete()
    }

    pub fn fraction(&self) -> Option<f64> {
        self.state.resume.fraction()
    }

    /// Feed one HLS segment; get the fMP4 bytes to append.
    pub fn push_segment(&mut self, bytes: &[u8]) -> Result<Vec<u8>, RemuxError> {
        match &mut self.remuxer {
            Some(r) => {
                let out = r.push_ts_segment(bytes)?;
                self.state.output_len += out.len() as u64;
                self.state.next_segment += 1;
                Ok(out)
            }
            // Not a remuxed job — an fMP4/CMAF variant is already in its final container,
            // so its segments are appended verbatim.
            None => {
                self.state.output_len += bytes.len() as u64;
                self.state.next_segment += 1;
                Ok(bytes.to_vec())
            }
        }
    }

    /// Also compute the eD2k hash of the read-back, for a job that has one to check.
    ///
    /// Called before the read-back begins; calling it later would hash only the tail.
    pub fn enable_ed2k(&mut self) {
        self.ed2k = Some(crate::ed2k::Ed2kHasher::new());
    }

    /// The eD2k hash of everything hashed, or `None` when it was never asked for.
    pub fn ed2k_hex(&self) -> Option<String> {
        self.ed2k.as_ref().map(|h| h.finish_hex())
    }

    pub fn hash_update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        if let Some(ed2k) = self.ed2k.as_mut() {
            ed2k.update(bytes);
        }
    }

    pub fn hash_hex(&self) -> String {
        self.hasher.finish_hex()
    }

    pub fn hashed_len(&self) -> u64 {
        self.hasher.len()
    }

    /// Start the verification pass over again, so a session can be re-verified without
    /// being rebuilt.
    pub fn reset_hash(&mut self) {
        self.hasher = Hasher::new();
        // Whether it was enabled is preserved; its accumulated bytes are not. Dropping
        // the flag here would silently stop verifying on the second attempt.
        if self.ed2k.is_some() {
            self.ed2k = Some(crate::ed2k::Ed2kHasher::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_survives_a_json_round_trip() {
        let mut s = SessionState::new(Some(1000), true, false);
        s.resume.record(ByteRange { start: 0, end: 99 });
        s.resume.validator = Some("\"abc123\"".into());
        let restored = SessionState::from_json(&s.to_json()).unwrap();
        assert_eq!(restored.resume.completed, s.resume.completed);
        assert_eq!(restored.resume.total, Some(1000));
        assert_eq!(restored.resume.validator.as_deref(), Some("\"abc123\""));
    }

    #[test]
    fn a_restored_session_plans_only_what_is_still_missing() {
        let mut s = DownloadSession::new(Some(1000), true, false, false);
        s.record(0, 499);
        let json = s.state_json();

        let restored = DownloadSession::restore(&json).unwrap();
        assert_eq!(restored.downloaded(), 500);
        assert_eq!(
            restored.plan(500, 1),
            vec![ByteRange {
                start: 500,
                end: 999
            }]
        );
    }

    #[test]
    fn a_remuxing_session_carries_its_remuxer_state_through_a_restore() {
        let mut s = DownloadSession::new(None, false, true, false);
        // A non-TS payload fails to remux, which is fine here — what matters is that
        // the carry-over is captured and rebuilt, not that this particular byte string
        // produces a fragment.
        let _ = s.push_segment(&[0u8; 8]);
        let json = s.state_json();
        assert!(
            json.contains("remux_state"),
            "the remuxer carry-over must be persisted, or HLS resume silently restarts"
        );

        let restored = DownloadSession::restore(&json).unwrap();
        assert!(restored.state().remux);
        // Round-trip identity: a session rebuilt from a snapshot must produce that
        // same snapshot, or something in the carry-over was dropped on the way through.
        assert_eq!(restored.state_json(), json);
    }

    #[test]
    fn a_non_remuxing_session_persists_no_remuxer_state() {
        let s = DownloadSession::new(Some(10), true, false, false);
        let restored = DownloadSession::restore(&s.state_json()).unwrap();
        assert_eq!(restored.state().remux_state, None);
    }

    #[test]
    fn segment_progress_survives_a_restore() {
        let mut s = DownloadSession::new(None, false, false, false);
        s.push_segment(&[1, 2, 3]).unwrap();
        s.push_segment(&[4, 5, 6]).unwrap();
        let restored = DownloadSession::restore(&s.state_json()).unwrap();
        assert_eq!(
            restored.next_segment(),
            2,
            "a resumed job must skip what it already fetched"
        );
        assert_eq!(
            restored.output_len(),
            6,
            "and resume writing at the right offset"
        );
    }

    #[test]
    fn malformed_persisted_state_is_an_error_not_a_panic() {
        assert!(DownloadSession::restore("not json").is_err());
        assert!(DownloadSession::restore("{}").is_err());
    }

    #[test]
    fn a_non_remuxing_session_passes_segments_through_untouched() {
        let mut s = DownloadSession::new(None, false, false, false);
        let out = s.push_segment(&[1, 2, 3, 4]).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4]);
        assert_eq!(s.output_len(), 4);
        assert_eq!(s.next_segment(), 1);
    }

    #[test]
    fn hashing_is_incremental_and_reportable_mid_stream() {
        let mut s = DownloadSession::new(Some(3), false, false, false);
        s.hash_update(b"a");
        let after_one = s.hash_hex();
        s.hash_update(b"bc");
        assert_eq!(s.hashed_len(), 3);
        assert_ne!(after_one, s.hash_hex());
        // Reporting the digest must not consume the hasher.
        assert_eq!(s.hash_hex(), s.hash_hex());
    }

    #[test]
    fn resetting_the_hash_allows_a_second_verification_pass() {
        let mut s = DownloadSession::new(Some(3), false, false, false);
        s.hash_update(b"abc");
        let first = s.hash_hex();
        s.reset_hash();
        assert_eq!(s.hashed_len(), 0);
        s.hash_update(b"abc");
        assert_eq!(s.hash_hex(), first);
    }
}
