//! A newline-delimited JSON window: one JSON object per line.

use serde_json::Value;
use zirv_flow_core::csv_file::MAX_RECORD_BYTES;

use super::{Parsed, error};
use crate::sources::SourceError;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Reads one window of an NDJSON file, which begins at a line start.
pub(super) struct NdjsonWindow {
    line: Vec<u8>,
    /// Bytes of the window fed so far.
    fed: u64,
    /// Bytes of the window up to the end of the last complete line.
    consumed: u64,
    /// Whether the window is the file's first, which may begin with a byte order mark.
    at_start: bool,
    /// The record the next one is, counting from 1.
    ordinal: u64,
}

impl NdjsonWindow {
    pub(super) fn new(at_start: bool, records: u64) -> Self {
        Self {
            line: Vec::new(),
            fed: 0,
            consumed: 0,
            at_start,
            ordinal: records,
        }
    }

    pub(super) fn ordinal(&self) -> u64 {
        self.ordinal
    }

    pub(super) fn consumed(&self) -> u64 {
        self.consumed
    }

    pub(super) fn push(&mut self, slice: &[u8], out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        for &byte in slice {
            self.fed += 1;
            if byte == b'\n' {
                self.consumed = self.fed;
                self.complete(out)?;
            } else {
                if self.line.len() >= MAX_RECORD_BYTES {
                    return Err(error::record_too_large());
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }

    /// The last line of the file, which needs no line end.
    pub(super) fn finish(&mut self, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        self.consumed = self.fed;
        self.complete(out)
    }

    fn complete(&mut self, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        let mut line = std::mem::take(&mut self.line);
        if self.at_start {
            self.at_start = false;
            if line.starts_with(&BOM) {
                line.drain(..BOM.len());
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let Value::Object(record) =
            serde_json::from_slice::<Value>(&line).map_err(|_| error::json_invalid())?
        else {
            return Err(error::json_invalid());
        };
        self.ordinal += 1;
        out.push(Parsed {
            record,
            end: self.fed,
            ordinal: self.ordinal,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: &[u8], piece: usize) -> Result<Vec<Parsed>, SourceError> {
        let mut window = NdjsonWindow::new(true, 0);
        let mut out = Vec::new();
        for slice in bytes.chunks(piece) {
            window.push(slice, &mut out)?;
        }
        window.finish(&mut out)?;
        Ok(out)
    }

    #[test]
    fn each_line_is_a_record_with_blank_lines_crlf_and_a_last_line_without_a_line_end() {
        let bytes = b"\xEF\xBB\xBF{\"a\":1}\r\n\r\n  \n{\"a\":\"x]\\\"\"}\n{\"a\":3}";
        for piece in 1..=bytes.len() {
            let records = run(bytes, piece).unwrap();

            let values: Vec<_> = records
                .iter()
                .map(|r| Value::Object(r.record.clone()))
                .collect();
            assert_eq!(
                values,
                [
                    serde_json::json!({"a": 1}),
                    serde_json::json!({"a": "x]\""}),
                    serde_json::json!({"a": 3})
                ],
                "piece {piece}"
            );
            let ordinals: Vec<_> = records.iter().map(|r| r.ordinal).collect();
            assert_eq!(ordinals, [1, 2, 3]);
            assert_eq!(records[2].end as usize, bytes.len());
        }
    }

    #[test]
    fn a_partial_last_line_is_not_consumed_until_the_file_ends() {
        let mut window = NdjsonWindow::new(false, 0);
        let mut out = Vec::new();

        window.push(b"{\"a\":1}\n{\"a\"", &mut out).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(window.consumed(), 8);
    }

    #[test]
    fn a_line_that_is_not_an_object_or_not_json_is_invalid_and_never_quoted() {
        for bad in [&b"[1]\n"[..], b"{\"a\":\n", b"secret-text\n", b"\"s\"\n"] {
            let error = run(bad, 4).err().unwrap();

            assert_eq!(error, SourceError::Other("json_invalid".into()));
        }
    }

    #[test]
    fn a_line_over_a_mebibyte_is_too_large() {
        let mut bytes = b"{\"a\":\"".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_RECORD_BYTES));
        bytes.extend_from_slice(b"\"}\n");

        assert_eq!(
            run(&bytes, 64 * 1024).err(),
            Some(SourceError::Other("record_too_large".into()))
        );
    }
}
