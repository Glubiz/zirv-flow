//! A JSON window: the objects of a top-level array, found by a byte scanner that tracks only
//! string and nesting state, so a `]`, a `,` or a quote inside a string never splits a record.

use serde_json::Value;
use zirv_flow_core::csv_file::MAX_RECORD_BYTES;

use super::{Parsed, error};
use crate::sources::SourceError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Before the opening bracket.
    Start,
    /// After the opening bracket: an element or the closing bracket.
    BeforeElement,
    /// Inside an element.
    Element,
    /// After an element: a comma or the closing bracket.
    AfterElement,
    /// After a comma: an element.
    AfterComma,
    /// After the closing bracket: only whitespace.
    Done,
}

/// Reads one window of a JSON array of objects. A window at the start of the file expects the
/// opening bracket; one that resumes begins right after an element.
pub(super) struct JsonArrayWindow {
    state: State,
    element: Vec<u8>,
    depth: u32,
    in_string: bool,
    escaped: bool,
    /// Bytes of the window fed so far.
    fed: u64,
    /// Bytes of the window up to the end of the last element.
    consumed: u64,
    /// Whether the window is the file's first, which may begin with a byte order mark.
    at_start: bool,
    /// The record the next one is, counting from 1.
    ordinal: u64,
}

impl JsonArrayWindow {
    pub(super) fn new(at_start: bool, records: u64) -> Self {
        Self {
            state: if at_start {
                State::Start
            } else {
                State::AfterElement
            },
            element: Vec::new(),
            depth: 0,
            in_string: false,
            escaped: false,
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
            self.step(byte, out)?;
        }
        Ok(())
    }

    /// The end of the file: the array must have been closed.
    pub(super) fn finish(&mut self, _: &mut Vec<Parsed>) -> Result<(), SourceError> {
        match self.state {
            State::Done => Ok(()),
            State::Start if self.fed == 0 => Ok(()),
            State::Start => Err(error::json_not_an_array()),
            _ => Err(error::json_invalid()),
        }
    }

    fn step(&mut self, byte: u8, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        if self.state == State::Element {
            return self.in_element(byte, out);
        }
        if byte.is_ascii_whitespace() {
            return Ok(());
        }
        match (self.state, byte) {
            // A byte order mark.
            (State::Start, 0xEF | 0xBB | 0xBF) if self.at_start && self.fed <= 3 => {}
            (State::Start, b'[') => self.state = State::BeforeElement,
            (State::Start, _) => return Err(error::json_not_an_array()),
            (State::BeforeElement | State::AfterComma, b'{') => self.begin_element(),
            (State::BeforeElement | State::AfterElement, b']') => self.state = State::Done,
            (State::AfterElement, b',') => self.state = State::AfterComma,
            (State::Done, _) => return Err(error::json_trailing_data()),
            _ => return Err(error::json_invalid()),
        }
        Ok(())
    }

    fn begin_element(&mut self) {
        self.state = State::Element;
        self.element.clear();
        self.element.push(b'{');
        self.depth = 1;
        self.in_string = false;
        self.escaped = false;
    }

    fn in_element(&mut self, byte: u8, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        if self.element.len() >= MAX_RECORD_BYTES {
            return Err(error::record_too_large());
        }
        self.element.push(byte);
        if self.in_string {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => self.in_string = true,
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => {
                self.depth -= 1;
                if self.depth == 0 {
                    self.complete(out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn complete(&mut self, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        let Value::Object(record) =
            serde_json::from_slice::<Value>(&self.element).map_err(|_| error::json_invalid())?
        else {
            return Err(error::json_invalid());
        };
        self.element.clear();
        self.state = State::AfterElement;
        self.consumed = self.fed;
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
    use serde_json::json;

    use super::*;

    fn run(bytes: &[u8], piece: usize) -> Result<Vec<Parsed>, SourceError> {
        let mut window = JsonArrayWindow::new(true, 0);
        let mut out = Vec::new();
        for slice in bytes.chunks(piece) {
            window.push(slice, &mut out)?;
        }
        window.finish(&mut out)?;
        Ok(out)
    }

    fn values(records: &[Parsed]) -> Vec<Value> {
        records
            .iter()
            .map(|record| Value::Object(record.record.clone()))
            .collect()
    }

    #[test]
    fn hostile_strings_and_nesting_do_not_split_a_record_wherever_the_input_is_cut() {
        let text = concat!(
            "\u{FEFF} [ {\"a\": \"]}, \\\" \\\\\", \"n\": {\"x\": [1, {\"y\": \"[\"}]}} ,\n",
            "{\"a\": \"é\"},{}\n]\n"
        );
        let bytes = text.as_bytes();
        for piece in 1..=bytes.len() {
            let records = run(bytes, piece).unwrap();

            assert_eq!(
                values(&records),
                [
                    json!({"a": "]}, \" \\", "n": {"x": [1, {"y": "["}]}}),
                    json!({"a": "é"}),
                    json!({})
                ],
                "piece {piece}"
            );
            let ordinals: Vec<_> = records.iter().map(|r| r.ordinal).collect();
            assert_eq!(ordinals, [1, 2, 3]);
        }
    }

    #[test]
    fn an_empty_array_and_an_empty_file_hold_no_records() {
        assert!(run(b"[]", 1).unwrap().is_empty());
        assert!(run(b" [ ] \n", 2).unwrap().is_empty());
        assert!(run(b"", 1).unwrap().is_empty());
    }

    #[test]
    fn what_is_not_an_array_of_objects_is_refused_with_a_code() {
        let code = |bytes: &[u8]| match run(bytes, 3).err().unwrap() {
            SourceError::Other(code) => code,
            other => panic!("{other:?}"),
        };

        assert_eq!(code(b"{\"a\":1}"), "json_not_an_array");
        assert_eq!(code(b"   "), "json_not_an_array");
        assert_eq!(code(b"[1,2]"), "json_invalid");
        assert_eq!(code(b"[{\"a\":1},]"), "json_invalid");
        assert_eq!(code(b"[,{}]"), "json_invalid");
        assert_eq!(code(b"[{\"a\":1}{\"a\":2}]"), "json_invalid");
        assert_eq!(code(b"[{\"a\":1}"), "json_invalid");
        assert_eq!(code(b"[{\"a\":secret}]"), "json_invalid");
        assert_eq!(code(b"[{\"a\":1}] x"), "json_trailing_data");
        assert_eq!(code(b"[{\"a\":1}]]"), "json_trailing_data");
    }

    #[test]
    fn an_element_over_a_mebibyte_is_too_large() {
        let mut bytes = b"[{\"a\":\"".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_RECORD_BYTES));
        bytes.extend_from_slice(b"\"}]");

        assert_eq!(
            run(&bytes, 64 * 1024).err(),
            Some(SourceError::Other("record_too_large".into()))
        );
    }

    #[test]
    fn a_resumed_window_starts_after_an_element_and_reports_where_the_last_one_ended() {
        let mut window = JsonArrayWindow::new(false, 4);
        let mut out = Vec::new();

        window.push(b" ,{\"a\":1},{\"a\":", &mut out).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ordinal, 5);
        assert_eq!(out[0].end, 9);
        assert_eq!(window.consumed(), 9);
    }
}
