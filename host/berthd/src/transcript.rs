//! What a session wrote: records of (stream, bytes) in arrival order, with
//! byte offsets and line numbers that count from the start of the session even
//! after the oldest output has been dropped.

use std::collections::VecDeque;

/// Bytes kept per session.
pub const CAP: usize = 16 << 20;

/// The selection `merged`; 1 is standard output (or a terminal), 2 standard error.
pub const MERGED: usize = 0;

const BASE: u8 = 0xff;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct Counters {
    written: [u64; 3],
    dropped: [u64; 3],
    lines: [u64; 3],
    dropped_lines: [u64; 3],
}

pub struct Transcript {
    cap: usize,
    records: VecDeque<(u8, Vec<u8>)>,
    kept: usize,
    counters: Counters,
}

/// Lines read back, and where they are.
pub struct Lines {
    pub lines: Vec<Vec<u8>>,
    /// The number of the first line returned.
    pub first: u64,
    /// The byte offset the first returned line starts at, and the one after the last.
    pub offset: u64,
    pub next_offset: u64,
}

fn newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|byte| **byte == b'\n').count() as u64
}

impl Transcript {
    pub fn new(cap: usize) -> Transcript {
        Transcript {
            cap,
            records: VecDeque::new(),
            kept: 0,
            counters: Counters::default(),
        }
    }

    /// Add output. Returns the merged offset it starts at.
    pub fn append(&mut self, stream: u8, bytes: &[u8]) -> u64 {
        let stream = if stream == 2 { 2 } else { 1 };
        let at = self.counters.written[MERGED];
        let lines = newlines(bytes);
        for selection in [MERGED, stream as usize] {
            self.counters.written[selection] += bytes.len() as u64;
            self.counters.lines[selection] += lines;
        }
        self.kept += bytes.len();
        self.records.push_back((stream, bytes.to_vec()));
        while self.kept > self.cap && self.records.len() > 1 {
            let (stream, old) = self.records.pop_front().expect("not empty");
            self.kept -= old.len();
            let lines = newlines(&old);
            for selection in [MERGED, stream as usize] {
                self.counters.dropped[selection] += old.len() as u64;
                self.counters.dropped_lines[selection] += lines;
            }
        }
        at
    }

    pub fn written(&self, selection: usize) -> u64 {
        self.counters.written[selection]
    }

    /// Where what is still kept begins.
    pub fn first(&self, selection: usize) -> u64 {
        self.counters.dropped[selection]
    }

    /// Completed lines so far.
    pub fn lines(&self, selection: usize) -> u64 {
        self.counters.lines[selection]
    }

    fn selected(&self, selection: usize) -> impl Iterator<Item = &(u8, Vec<u8>)> {
        self.records
            .iter()
            .filter(move |(stream, _)| selection == MERGED || *stream as usize == selection)
    }

    /// Up to `limit` bytes of a selection from `since`. Returns where it starts.
    pub fn read(&self, selection: usize, since: u64, limit: usize) -> (u64, Vec<u8>) {
        let start = since.clamp(self.first(selection), self.written(selection));
        let mut at = self.first(selection);
        let mut out = vec![];
        for (_, bytes) in self.selected(selection) {
            let end = at + bytes.len() as u64;
            if end > start && out.len() < limit {
                let skip = start.saturating_sub(at) as usize;
                let take = (bytes.len() - skip).min(limit - out.len());
                out.extend_from_slice(&bytes[skip..skip + take]);
            }
            at = end;
        }
        (start, out)
    }

    /// The records from a merged offset on, for replaying to a viewer.
    pub fn frames(&self, since: u64) -> Vec<(u8, Vec<u8>)> {
        let start = since.max(self.first(MERGED));
        let mut at = self.first(MERGED);
        let mut out = vec![];
        for (stream, bytes) in &self.records {
            let end = at + bytes.len() as u64;
            if end > start {
                out.push((*stream, bytes[start.saturating_sub(at) as usize..].to_vec()));
            }
            at = end;
        }
        out
    }

    /// `count` lines from line `from`, or the last `count` when `from` is None.
    /// A last line without its end counts only once nothing more can come.
    pub fn read_lines(
        &self,
        selection: usize,
        from: Option<u64>,
        count: usize,
        ended: bool,
    ) -> Lines {
        let (base, data) = self.read(selection, 0, usize::MAX);
        let first_line = self.counters.dropped_lines[selection];
        // (start, end) of each line, without its newline.
        let mut spans = vec![];
        let mut start = 0;
        for (index, byte) in data.iter().enumerate() {
            if *byte == b'\n' {
                spans.push((start, index));
                start = index + 1;
            }
        }
        if ended && start < data.len() {
            spans.push((start, data.len()));
        }
        let total = spans.len();
        let skip = match from {
            Some(line) => (line.saturating_sub(first_line) as usize).min(total),
            None => total.saturating_sub(count),
        };
        let chosen = &spans[skip..(skip + count).min(total)];
        let offset = chosen
            .first()
            .map_or(base + data.len().min(start) as u64, |span| {
                base + span.0 as u64
            });
        let next_offset = match chosen.last() {
            Some(span) => base + (span.1 + 1).min(data.len()) as u64,
            None => offset,
        };
        Lines {
            lines: chosen
                .iter()
                .map(|(start, end)| data[*start..*end].to_vec())
                .collect(),
            first: first_line + skip as u64,
            offset,
            next_offset,
        }
    }

    /// One record as it is stored.
    pub fn record(stream: u8, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + 5);
        out.push(stream);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        out
    }

    /// Everything kept, as a file that `load` reads back to the same state.
    pub fn store(&self) -> Vec<u8> {
        // The counters as they were before the kept records.
        let mut base = self.counters.clone();
        for selection in 0..3 {
            base.written[selection] = base.dropped[selection];
            base.lines[selection] = base.dropped_lines[selection];
        }
        let mut out = Self::record(BASE, &serde_json::to_vec(&base).expect("counters"));
        for (stream, bytes) in &self.records {
            out.extend(Self::record(*stream, bytes));
        }
        out
    }

    /// Read a stored transcript. A record cut short by a crash is left out.
    pub fn load(data: &[u8], cap: usize) -> Transcript {
        let mut transcript = Transcript::new(cap);
        let mut rest = data;
        while rest.len() >= 5 {
            let length = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
            if rest.len() < 5 + length {
                break;
            }
            let (stream, bytes) = (rest[0], &rest[5..5 + length]);
            if stream == BASE {
                if let Ok(counters) = serde_json::from_slice(bytes) {
                    transcript = Transcript {
                        cap,
                        records: VecDeque::new(),
                        kept: 0,
                        counters,
                    };
                }
            } else {
                transcript.append(stream, bytes);
            }
            rest = &rest[5 + length..];
        }
        transcript
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_count_per_stream_and_merged() {
        let mut log = Transcript::new(CAP);
        assert_eq!(log.append(1, b"one\ntw"), 0);
        assert_eq!(log.append(2, b"oops\n"), 6);
        assert_eq!(log.append(1, b"o\nthree"), 11);
        assert_eq!(log.read(1, 0, 100), (0, b"one\ntwo\nthree".to_vec()));
        assert_eq!(log.read(2, 2, 100), (2, b"ps\n".to_vec()));
        assert_eq!(log.read(MERGED, 4, 5), (4, b"twoop".to_vec()));
        assert_eq!((log.lines(1), log.lines(2), log.lines(MERGED)), (2, 1, 3));
        assert_eq!(
            log.frames(8),
            vec![(2, b"ps\n".to_vec()), (1, b"o\nthree".to_vec())]
        );
        // A half-written last line is not a line until the session has ended.
        let lines = log.read_lines(1, Some(1), 10, false);
        assert_eq!((lines.lines, lines.first), (vec![b"two".to_vec()], 1));
        assert_eq!((lines.offset, lines.next_offset), (4, 8));
        let lines = log.read_lines(1, None, 2, true);
        assert_eq!(lines.lines, vec![b"two".to_vec(), b"three".to_vec()]);
        assert_eq!((lines.first, lines.next_offset), (1, 13));
        assert!(log.read_lines(1, Some(9), 5, false).lines.is_empty());
    }

    #[test]
    fn the_oldest_is_dropped_and_the_numbers_stay() {
        let mut log = Transcript::new(10);
        for index in 0..6 {
            log.append(1, format!("l{index}\n").as_bytes());
        }
        assert_eq!((log.first(1), log.written(1)), (9, 18));
        assert_eq!(log.read(1, 0, 100), (9, b"l3\nl4\nl5\n".to_vec()));
        let lines = log.read_lines(1, Some(0), 2, false);
        assert_eq!((lines.first, lines.lines.len()), (3, 2));
        let again = Transcript::load(&log.store(), 10);
        assert_eq!(again.counters, log.counters);
        assert_eq!(again.read(1, 12, 100), (12, b"l4\nl5\n".to_vec()));
        // A cut-off record at the end is ignored.
        let mut stored = log.store();
        stored.extend_from_slice(&[1, 200, 0, 0, 0, b'x']);
        assert_eq!(Transcript::load(&stored, 10).written(1), 18);
    }
}
