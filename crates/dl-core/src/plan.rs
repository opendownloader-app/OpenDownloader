//! Resume state and range planning — the part of the system that makes an interrupted
//! download resumable rather than restartable.
//!
//! The invariant everything else depends on: for a file of known size, the union of the
//! completed ranges and the planned ranges always covers `0..total` exactly, with no gap
//! and no double-fetch. That is enforced by construction ([`ResumeState::missing`]
//! returns the exact complement of a normalized range set) and checked by a randomized
//! sweep in the tests.

use serde::{Deserialize, Serialize};

/// A closed byte interval, `start..=end`, matching HTTP's `Range` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        // Saturating: an open-ended range (end == u64::MAX) would otherwise overflow.
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    /// The value for an HTTP `Range` request header.
    pub fn header_value(&self) -> String {
        if self.end == u64::MAX {
            format!("bytes={}-", self.start)
        } else {
            format!("bytes={}-{}", self.start, self.end)
        }
    }
}

/// Everything needed to pick up a download where it left off.
///
/// `validator` is the server's `ETag` or `Last-Modified`. It is sent back as `If-Range`
/// on every resumed request: if the resource changed server-side, the server answers
/// `200` with the whole new body instead of `206`, and the caller must restart rather
/// than splice two different files together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeState {
    pub total: Option<u64>,
    pub validator: Option<String>,
    pub accepts_ranges: bool,
    /// Always normalized: sorted by `start`, non-overlapping, adjacent runs merged.
    pub completed: Vec<ByteRange>,
}

impl ResumeState {
    pub fn new(total: Option<u64>, accepts_ranges: bool) -> Self {
        Self {
            total,
            validator: None,
            accepts_ranges,
            completed: Vec::new(),
        }
    }

    pub fn with_validator(mut self, validator: Option<String>) -> Self {
        self.validator = validator;
        self
    }

    /// Record a range that has been successfully written to the sink.
    ///
    /// Overlapping and adjacent ranges are merged, so recording the same bytes twice is
    /// harmless — which matters, because a retried chunk may partially duplicate one
    /// that already landed.
    pub fn record(&mut self, r: ByteRange) {
        if r.is_empty() {
            return;
        }
        let r = match self.total {
            // Clamp to the known size so an open-ended fetch can be recorded verbatim.
            Some(t) if t > 0 => ByteRange {
                start: r.start.min(t - 1),
                end: r.end.min(t - 1),
            },
            _ => r,
        };
        self.completed.push(r);
        self.normalize();
    }

    fn normalize(&mut self) {
        self.completed.sort_unstable();
        let mut merged: Vec<ByteRange> = Vec::with_capacity(self.completed.len());
        for r in self.completed.drain(..) {
            match merged.last_mut() {
                // `cur.end + 1` makes adjacency merge too, not just overlap.
                Some(cur) if r.start <= cur.end.saturating_add(1) => {
                    cur.end = cur.end.max(r.end);
                }
                _ => merged.push(r),
            }
        }
        self.completed = merged;
    }

    /// The exact complement of [`Self::completed`] within `0..total`.
    ///
    /// With an unknown total this returns a single open-ended range starting after
    /// whatever contiguous prefix exists.
    pub fn missing(&self) -> Vec<ByteRange> {
        let Some(total) = self.total else {
            let start = self
                .completed
                .first()
                .filter(|r| r.start == 0)
                .map_or(0, |r| r.end.saturating_add(1));
            return vec![ByteRange {
                start,
                end: u64::MAX,
            }];
        };
        if total == 0 {
            return Vec::new();
        }

        let mut gaps = Vec::new();
        let mut cursor = 0u64;
        for r in &self.completed {
            if r.start > cursor {
                gaps.push(ByteRange {
                    start: cursor,
                    end: r.start - 1,
                });
            }
            cursor = cursor.max(r.end.saturating_add(1));
        }
        if cursor < total {
            gaps.push(ByteRange {
                start: cursor,
                end: total - 1,
            });
        }
        gaps
    }

    /// Total bytes confirmed on disk.
    pub fn downloaded(&self) -> u64 {
        self.completed.iter().map(ByteRange::len).sum()
    }

    pub fn is_complete(&self) -> bool {
        match self.total {
            Some(0) => true,
            Some(t) => {
                self.completed.len() == 1
                    && self.completed[0]
                        == ByteRange {
                            start: 0,
                            end: t - 1,
                        }
            }
            None => false,
        }
    }

    /// Progress as a fraction, or `None` when the total size is unknown.
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(0) => Some(1.0),
            Some(t) => Some(self.downloaded() as f64 / t as f64),
            None => None,
        }
    }
}

/// Decide what to fetch next.
///
/// Returns at most `max_parallel` ranges of at most `chunk_size` bytes each, drawn from
/// the start of the missing set. A server that does not support ranges gets a single
/// whole-file request — resumption is impossible there and pretending otherwise would
/// silently corrupt the output.
pub fn plan_chunks(state: &ResumeState, chunk_size: u64, max_parallel: usize) -> Vec<ByteRange> {
    if state.is_complete() || max_parallel == 0 {
        return Vec::new();
    }

    if !state.accepts_ranges {
        return match state.total {
            Some(0) => Vec::new(),
            Some(t) => vec![ByteRange {
                start: 0,
                end: t - 1,
            }],
            None => vec![ByteRange {
                start: 0,
                end: u64::MAX,
            }],
        };
    }

    let chunk_size = chunk_size.max(1);
    let mut out = Vec::with_capacity(max_parallel);
    for gap in state.missing() {
        let mut start = gap.start;
        while start <= gap.end {
            if out.len() == max_parallel {
                return out;
            }
            // Open-ended gaps (unknown total) must not be sliced — we cannot know where
            // they stop, so the caller streams until the body ends.
            if gap.end == u64::MAX {
                out.push(ByteRange {
                    start,
                    end: u64::MAX,
                });
                return out;
            }
            let end = start.saturating_add(chunk_size - 1).min(gap.end);
            out.push(ByteRange { start, end });
            start = end + 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_merges_adjacent_and_overlapping_ranges() {
        let mut s = ResumeState::new(Some(100), true);
        s.record(ByteRange { start: 0, end: 9 });
        s.record(ByteRange { start: 10, end: 19 });
        s.record(ByteRange { start: 15, end: 24 });
        assert_eq!(s.completed, vec![ByteRange { start: 0, end: 24 }]);
        assert_eq!(s.downloaded(), 25);
    }

    #[test]
    fn record_is_idempotent() {
        let mut s = ResumeState::new(Some(100), true);
        s.record(ByteRange { start: 10, end: 19 });
        s.record(ByteRange { start: 10, end: 19 });
        assert_eq!(s.completed, vec![ByteRange { start: 10, end: 19 }]);
        assert_eq!(s.downloaded(), 10);
    }

    #[test]
    fn missing_is_the_exact_complement() {
        let mut s = ResumeState::new(Some(100), true);
        s.record(ByteRange { start: 10, end: 19 });
        s.record(ByteRange { start: 50, end: 59 });
        assert_eq!(
            s.missing(),
            vec![
                ByteRange { start: 0, end: 9 },
                ByteRange { start: 20, end: 49 },
                ByteRange { start: 60, end: 99 },
            ]
        );
    }

    #[test]
    fn plan_respects_chunk_size_and_parallelism() {
        let s = ResumeState::new(Some(1000), true);
        let chunks = plan_chunks(&s, 256, 3);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], ByteRange { start: 0, end: 255 });
        assert_eq!(
            chunks[2],
            ByteRange {
                start: 512,
                end: 767
            }
        );
    }

    #[test]
    fn plan_resumes_from_the_gaps_not_from_zero() {
        let mut s = ResumeState::new(Some(1000), true);
        s.record(ByteRange { start: 0, end: 499 });
        assert_eq!(
            plan_chunks(&s, 256, 2),
            vec![
                ByteRange {
                    start: 500,
                    end: 755
                },
                ByteRange {
                    start: 756,
                    end: 999
                },
            ]
        );
    }

    #[test]
    fn without_range_support_plan_is_one_whole_file_chunk() {
        let s = ResumeState::new(Some(1000), false);
        assert_eq!(
            plan_chunks(&s, 256, 4),
            vec![ByteRange { start: 0, end: 999 }]
        );
    }

    #[test]
    fn unknown_total_plans_a_single_open_ended_chunk() {
        let s = ResumeState::new(None, false);
        assert_eq!(
            plan_chunks(&s, 256, 4),
            vec![ByteRange {
                start: 0,
                end: u64::MAX
            }]
        );
        assert_eq!(plan_chunks(&s, 256, 4)[0].header_value(), "bytes=0-");
    }

    #[test]
    fn completed_file_plans_nothing() {
        let mut s = ResumeState::new(Some(10), true);
        s.record(ByteRange { start: 0, end: 9 });
        assert!(s.is_complete());
        assert!(plan_chunks(&s, 4, 4).is_empty());
        assert_eq!(s.fraction(), Some(1.0));
    }

    #[test]
    fn range_header_is_inclusive_on_both_ends() {
        assert_eq!(
            ByteRange { start: 0, end: 255 }.header_value(),
            "bytes=0-255"
        );
        assert_eq!(ByteRange { start: 0, end: 255 }.len(), 256);
    }

    /// The load-bearing invariant: whatever has already landed, plus whatever we plan
    /// next, must together account for every byte of the file exactly once. A gap means
    /// a silently truncated file; an overlap means wasted bandwidth and, with a seeking
    /// sink, potentially interleaved writes.
    #[test]
    fn planned_plus_completed_always_covers_the_file_exactly() {
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let total = 1 + next() % 5000;
            let mut s = ResumeState::new(Some(total), true);
            for _ in 0..(next() % 8) {
                let a = next() % total;
                let b = (a + next() % 500).min(total - 1);
                s.record(ByteRange { start: a, end: b });
            }
            let chunk = 1 + next() % 700;
            let mut covered = vec![0u32; total as usize];
            // The cap must never bind here, or a short plan would look like a coverage
            // gap when it is really just `max_parallel` doing its job. With chunk >= 1,
            // `total` chunks is always enough to finish the file.
            let cap = total as usize;
            for r in s
                .completed
                .iter()
                .copied()
                .chain(plan_chunks(&s, chunk, cap))
            {
                for i in r.start..=r.end.min(total - 1) {
                    covered[i as usize] += 1;
                }
            }
            assert!(
                covered.iter().all(|&c| c == 1),
                "coverage must be exactly one everywhere (total={total}, chunk={chunk})"
            );
        }
    }
}
