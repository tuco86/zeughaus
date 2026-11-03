//! Masks a job's secret values in the log the CI stores.
//!
//! The log is the run's WAL ([`zeughaus_terminal::wal`]): the magic, then
//! records of `u64 at_micros`, `u32 len` and the payload. Each occurrence of
//! a value in the payload stream becomes as many `*` bytes, so every byte
//! offset stays what it is in the file: the CI resumes from offsets of the
//! file. A value can be cut by a record boundary, so complete records are
//! held back until enough payload follows them that no occurrence starting
//! in them can still grow: at least the longest value's length minus one.

use zeughaus_terminal::wal::MAGIC;

/// The time and the length before each payload.
const HEADER: usize = 12;

/// The masking state of one run's log, fed from offset 0 of the file.
pub struct Masker {
    /// Longest first, so a value inside a longer one is masked by both.
    masks: Vec<Vec<u8>>,
    /// Payload a record needs after it before it is released.
    keep: usize,
    /// Fed and not released; starts at the file's start or a record's.
    pending: Vec<u8>,
    /// The magic has not been released yet.
    at_start: bool,
}

/// One record's payload in `pending`.
struct Span {
    start: usize,
    end: usize,
    complete: bool,
}

impl Masker {
    pub fn new(mut masks: Vec<Vec<u8>>) -> Masker {
        masks.retain(|mask| !mask.is_empty());
        masks.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        masks.dedup();
        let keep = masks.first().map_or(0, |mask| mask.len() - 1);
        Masker {
            masks,
            keep,
            pending: Vec::new(),
            at_start: true,
        }
    }

    /// The next raw bytes of the file.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The masked bytes that are safe to send, continuing where the last
    /// call ended.
    pub fn take_ready(&mut self) -> Vec<u8> {
        self.release(false)
    }

    /// Everything fed and not yet released, masked: the run has ended.
    pub fn flush(&mut self) -> Vec<u8> {
        self.release(true)
    }

    fn release(&mut self, all: bool) -> Vec<u8> {
        if self.masks.is_empty() {
            return std::mem::take(&mut self.pending);
        }
        let start = if self.at_start { MAGIC.len() } else { 0 };
        if self.pending.len() < start {
            // Not even the magic is complete: nothing to mask in it.
            return if all {
                std::mem::take(&mut self.pending)
            } else {
                Vec::new()
            };
        }
        let spans = self.spans(start);
        self.mask(&spans);
        let cut = if all {
            self.pending.len()
        } else {
            // The last complete record with enough payload after it.
            let mut after = 0usize;
            let mut cut = start;
            for span in spans.iter().rev().filter(|span| span.complete) {
                if after >= self.keep {
                    cut = span.end;
                    break;
                }
                after += span.end - span.start;
            }
            cut
        };
        self.at_start = false;
        self.pending.drain(..cut).collect()
    }

    /// The payloads in `pending` from `start` on; the last one may be cut
    /// short by what has been fed so far.
    fn spans(&self, start: usize) -> Vec<Span> {
        let data = &self.pending;
        let mut spans = Vec::new();
        let mut at = start;
        while let Some(header) = data.get(at..at + HEADER) {
            let len = u32::from_le_bytes(header[8..].try_into().expect("4 bytes")) as usize;
            let body = at + HEADER;
            let end = body + len;
            if end <= data.len() {
                spans.push(Span {
                    start: body,
                    end,
                    complete: true,
                });
                at = end;
            } else {
                spans.push(Span {
                    start: body,
                    end: data.len(),
                    complete: false,
                });
                break;
            }
        }
        spans
    }

    /// Replaces every occurrence of a mask in the concatenated payloads.
    fn mask(&mut self, spans: &[Span]) {
        let mut stream: Vec<u8> = Vec::new();
        for span in spans {
            stream.extend_from_slice(&self.pending[span.start..span.end]);
        }
        let mut hit = vec![false; stream.len()];
        for at in 0..stream.len() {
            for mask in &self.masks {
                if stream[at..].starts_with(mask) {
                    hit[at..at + mask.len()].fill(true);
                }
            }
        }
        if !hit.contains(&true) {
            return;
        }
        let mut next = 0;
        for span in spans {
            for byte in &mut self.pending[span.start..span.end] {
                if hit[next] {
                    *byte = b'*';
                }
                next += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal(records: &[&[u8]]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for (i, payload) in records.iter().enumerate() {
            out.extend_from_slice(&(i as u64).to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    /// The payloads of a masked WAL, joined.
    fn payloads(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = MAGIC.len();
        while at + HEADER <= data.len() {
            let len = u32::from_le_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
            out.extend_from_slice(&data[at + HEADER..at + HEADER + len]);
            at += HEADER + len;
        }
        out
    }

    /// Feeds `input` in pieces of `step` bytes, taking what is ready after
    /// each, then flushes.
    fn run(masks: &[&[u8]], input: &[u8], step: usize) -> Vec<u8> {
        let mut masker = Masker::new(masks.iter().map(|m| m.to_vec()).collect());
        let mut out = Vec::new();
        for piece in input.chunks(step) {
            masker.feed(piece);
            out.extend(masker.take_ready());
        }
        out.extend(masker.flush());
        out
    }

    #[test]
    fn a_value_inside_one_record_is_masked() {
        let input = wal(&[b"token: abc12345 done\r\n"]);
        let out = run(&[b"abc12345"], &input, input.len());
        assert_eq!(out.len(), input.len());
        assert_eq!(payloads(&out), b"token: ******** done\r\n");
    }

    #[test]
    fn a_value_split_across_two_records_is_masked() {
        let input = wal(&[b"plain:abc1", b"2345\r\n"]);
        for step in [1, 5, 13, input.len()] {
            let out = run(&[b"abc12345"], &input, step);
            assert_eq!(out.len(), input.len(), "step {step}");
            assert_eq!(payloads(&out), b"plain:********\r\n", "step {step}");
        }
    }

    #[test]
    fn a_value_split_across_three_short_records_is_masked() {
        let input = wal(&[b"xab", b"c1", b"2345y", b"z"]);
        for step in [1, 7, input.len()] {
            let out = run(&[b"abc12345", b"other-secret"], &input, step);
            assert_eq!(out.len(), input.len(), "step {step}");
            assert_eq!(payloads(&out), b"x********yz", "step {step}");
        }
    }

    #[test]
    fn records_are_held_back_only_until_enough_payload_follows() {
        let mut masker = Masker::new(vec![b"abcd".to_vec()]);
        let first = wal(&[b"ab"]);
        masker.feed(&first);
        // The magic goes at once; the record waits for 3 more payload bytes.
        assert_eq!(masker.take_ready(), MAGIC.to_vec());
        let rest = &wal(&[b"ab", b"cdef"])[first.len()..];
        masker.feed(rest);
        let ready = masker.take_ready();
        assert_eq!(ready.len(), HEADER + 2, "the first record is safe now");
        assert_eq!(&ready[HEADER..], b"**");
        assert_eq!(&masker.flush()[HEADER..], b"**ef");
    }

    #[test]
    fn without_masks_everything_passes_through_unchanged() {
        let input = wal(&[b"abc12345", b"more"]);
        let mut masker = Masker::new(Vec::new());
        masker.feed(&input[..10]);
        assert_eq!(masker.take_ready(), input[..10]);
        masker.feed(&input[10..]);
        assert_eq!(masker.take_ready(), input[10..]);
        assert!(masker.flush().is_empty());
    }
}
