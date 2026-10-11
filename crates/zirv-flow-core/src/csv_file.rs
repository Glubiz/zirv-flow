//! An incremental CSV reader for uploaded files (architecture section 7.2): bytes go in as
//! they arrive, records come out as soon as they are complete, and nothing holds the whole
//! file. RFC 4180 quoting, UTF-8, a byte order mark ignored at the start, LF, CRLF or CR
//! line ends, and blank lines skipped.
//!
//! Errors name the rule that was broken and never quote the file.

use std::fmt;

use csv_core::{ReadRecordResult, Reader, ReaderBuilder};

/// The most bytes one record may hold (all its fields together).
pub const MAX_RECORD_BYTES: usize = 1 << 20;

/// The most fields one record may hold.
pub const MAX_FIELDS: usize = 1000;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The delimiters a file may use.
pub const DELIMITERS: [u8; 3] = *b",;\t";

/// Why a file was refused as a whole.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CsvError {
    /// A field is not valid UTF-8.
    NotUtf8,
    /// One record is larger than [`MAX_RECORD_BYTES`], which an unclosed quote also causes.
    RecordTooLarge,
    /// One record has more than [`MAX_FIELDS`] fields.
    TooManyFields,
}

impl CsvError {
    /// A stable, static code.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotUtf8 => "not_utf8",
            Self::RecordTooLarge => "record_too_large",
            Self::TooManyFields => "too_many_fields",
        }
    }
}

impl fmt::Display for CsvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotUtf8 => "the file must be UTF-8 text",
            Self::RecordTooLarge => "a record is over 1 MiB; a quoted field may never be closed",
            Self::TooManyFields => "a record has more than 1000 fields",
        })
    }
}

impl std::error::Error for CsvError {}

/// Reads records from bytes fed in pieces of any size.
pub struct CsvParser {
    reader: Reader,
    output: Vec<u8>,
    ends: Vec<usize>,
    /// The part of the current record already read into `output` and `ends`.
    output_used: usize,
    ends_used: usize,
    /// Bytes of the file's start held back until it is clear whether they are a BOM.
    prefix: Vec<u8>,
    started: bool,
}

impl CsvParser {
    pub fn new(delimiter: u8) -> Self {
        Self {
            reader: ReaderBuilder::new().delimiter(delimiter).build(),
            output: vec![0; 8 * 1024],
            ends: vec![0; 64],
            output_used: 0,
            ends_used: 0,
            prefix: Vec::new(),
            started: false,
        }
    }

    /// Feeds `bytes`, handing each completed record to `on_record`.
    pub fn push(
        &mut self,
        bytes: &[u8],
        on_record: &mut impl FnMut(Vec<String>),
    ) -> Result<(), CsvError> {
        if self.started {
            return if bytes.is_empty() {
                Ok(())
            } else {
                self.read(bytes, on_record)
            };
        }
        self.prefix.extend_from_slice(bytes);
        if self.prefix.len() < BOM.len() && BOM.starts_with(&self.prefix) {
            // Still possibly a byte order mark: wait for more.
            return Ok(());
        }
        self.start(on_record)
    }

    /// Ends the file: a last record without a line end is handed over.
    pub fn finish(&mut self, on_record: &mut impl FnMut(Vec<String>)) -> Result<(), CsvError> {
        if !self.started {
            self.start(on_record)?;
        }
        self.read(&[], on_record)
    }

    /// Reads the held-back start of the file, without its byte order mark.
    fn start(&mut self, on_record: &mut impl FnMut(Vec<String>)) -> Result<(), CsvError> {
        self.started = true;
        let held = std::mem::take(&mut self.prefix);
        let body = held.strip_prefix(&BOM).unwrap_or(&held);
        if body.is_empty() {
            return Ok(());
        }
        self.read(body, on_record)
    }

    /// Reads records from `input` until it is used up; an empty `input` means the end.
    fn read(
        &mut self,
        mut input: &[u8],
        on_record: &mut impl FnMut(Vec<String>),
    ) -> Result<(), CsvError> {
        loop {
            let (result, read, written, ended) = self.reader.read_record(
                input,
                &mut self.output[self.output_used..],
                &mut self.ends[self.ends_used..],
            );
            input = &input[read..];
            self.output_used += written;
            self.ends_used += ended;
            match result {
                ReadRecordResult::InputEmpty => return Ok(()),
                ReadRecordResult::OutputFull => {
                    if self.output.len() >= MAX_RECORD_BYTES {
                        return Err(CsvError::RecordTooLarge);
                    }
                    let grown = (self.output.len() * 2).min(MAX_RECORD_BYTES);
                    self.output.resize(grown, 0);
                }
                ReadRecordResult::OutputEndsFull => {
                    if self.ends.len() >= MAX_FIELDS {
                        return Err(CsvError::TooManyFields);
                    }
                    let grown = (self.ends.len() * 2).min(MAX_FIELDS);
                    self.ends.resize(grown, 0);
                }
                ReadRecordResult::Record => {
                    let record = fields(&self.output, &self.ends[..self.ends_used])?;
                    self.output_used = 0;
                    self.ends_used = 0;
                    on_record(record);
                }
                ReadRecordResult::End => return Ok(()),
            }
        }
    }
}

/// The fields of a record, as text.
fn fields(output: &[u8], ends: &[usize]) -> Result<Vec<String>, CsvError> {
    let mut fields = Vec::with_capacity(ends.len());
    let mut start = 0;
    for end in ends {
        let text = std::str::from_utf8(&output[start..*end]).map_err(|_| CsvError::NotUtf8)?;
        fields.push(text.to_owned());
        start = *end;
    }
    Ok(fields)
}

/// The delimiter of a file, judged from its first line: the one of `,`, `;` and tab that
/// occurs most outside quotes, the comma on a tie or when none occurs. `sample` is the start
/// of the file; a byte order mark is ignored.
pub fn detect_delimiter(sample: &[u8]) -> u8 {
    let sample = sample.strip_prefix(&BOM).unwrap_or(sample);
    let mut counts = [0_usize; 3];
    let mut quoted = false;
    for byte in sample {
        match byte {
            b'"' => quoted = !quoted,
            b'\n' | b'\r' if !quoted => break,
            _ if quoted => {}
            other => {
                if let Some(index) = DELIMITERS.iter().position(|d| d == other) {
                    counts[index] += 1;
                }
            }
        }
    }
    let mut best = 0;
    for index in 1..counts.len() {
        if counts[index] > counts[best] {
            best = index;
        }
    }
    DELIMITERS[best]
}

/// Writes one field: quoted when it holds the delimiter, a quote or a line break.
pub fn write_field(out: &mut String, value: &str, delimiter: char) {
    if value.contains([delimiter, '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&value.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_in(pieces: &[&[u8]], delimiter: u8) -> Result<Vec<Vec<String>>, CsvError> {
        let mut parser = CsvParser::new(delimiter);
        let mut records = Vec::new();
        let mut take = |record: Vec<String>| records.push(record);
        for piece in pieces {
            parser.push(piece, &mut take)?;
        }
        parser.finish(&mut take)?;
        Ok(records)
    }

    fn rows(records: Vec<Vec<String>>) -> Vec<Vec<&'static str>> {
        records
            .into_iter()
            .map(|record| {
                record
                    .into_iter()
                    .map(|field| &*Box::leak(field.into_boxed_str()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn records_split_on_the_delimiter_and_line_ends() {
        let records = parse_in(&[b"a,b\r\n1,2\n3,4"], b',').unwrap();

        assert_eq!(rows(records), [["a", "b"], ["1", "2"], ["3", "4"]]);
    }

    #[test]
    fn quoted_fields_hold_delimiters_quotes_and_line_breaks() {
        let records = parse_in(&[b"a,b\n\"x,\"\"y\"\"\nz\",2\n"], b',').unwrap();

        assert_eq!(rows(records), [vec!["a", "b"], vec!["x,\"y\"\nz", "2"]]);
    }

    #[test]
    fn a_record_split_across_pieces_comes_out_whole() {
        let whole = b"a,b\n\"one\ntwo\",2\n3,4\n";
        for cut in 1..whole.len() {
            let records = parse_in(&[&whole[..cut], &whole[cut..]], b',').unwrap();

            assert_eq!(
                rows(records),
                [vec!["a", "b"], vec!["one\ntwo", "2"], vec!["3", "4"]],
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_byte_order_mark_is_ignored_however_it_is_split() {
        let whole = b"\xEF\xBB\xBFa,b\n1,2\n";
        for cut in 1..5 {
            let records = parse_in(&[&whole[..cut], &whole[cut..]], b',').unwrap();

            assert_eq!(rows(records), [["a", "b"], ["1", "2"]], "cut at {cut}");
        }
        assert!(parse_in(&[&BOM], b',').unwrap().is_empty());
    }

    #[test]
    fn blank_lines_are_skipped() {
        let records = parse_in(&[b"a,b\n\n1,2\n\n"], b',').unwrap();

        assert_eq!(rows(records), [["a", "b"], ["1", "2"]]);
    }

    #[test]
    fn semicolons_and_tabs_are_delimiters_when_asked() {
        assert_eq!(
            rows(parse_in(&[b"a;b\n1;2\n"], b';').unwrap()),
            [["a", "b"], ["1", "2"]]
        );
        assert_eq!(
            rows(parse_in(&[b"a\tb\n1,5\t2\n"], b'\t').unwrap()),
            [["a", "b"], ["1,5", "2"]]
        );
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert_eq!(parse_in(&[b"a\n\xFF\xFE\n"], b','), Err(CsvError::NotUtf8));
    }

    #[test]
    fn a_record_over_the_cap_and_an_unclosed_quote_are_refused() {
        let big = vec![b'x'; MAX_RECORD_BYTES + 10];

        assert_eq!(parse_in(&[&big], b','), Err(CsvError::RecordTooLarge));
        let mut unclosed = b"a\n\"".to_vec();
        unclosed.extend(vec![b'y'; MAX_RECORD_BYTES + 10]);
        assert_eq!(parse_in(&[&unclosed], b','), Err(CsvError::RecordTooLarge));
    }

    #[test]
    fn a_record_with_too_many_fields_is_refused() {
        let wide = vec![b','; MAX_FIELDS + 5];

        assert_eq!(parse_in(&[&wide], b','), Err(CsvError::TooManyFields));
        let ok = vec![b','; MAX_FIELDS - 1];
        assert_eq!(parse_in(&[&ok], b',').unwrap()[0].len(), MAX_FIELDS);
    }

    #[test]
    fn the_delimiter_is_the_most_common_one_outside_quotes() {
        assert_eq!(detect_delimiter(b"a,b,c\n1;2;3;4"), b',');
        assert_eq!(detect_delimiter(b"a;b;c\n"), b';');
        assert_eq!(detect_delimiter(b"a\tb\tc\n"), b'\t');
        assert_eq!(detect_delimiter(b"\"a,b,c\";d\n"), b';');
        assert_eq!(detect_delimiter(b"single\n"), b',');
        assert_eq!(detect_delimiter(b"\xEF\xBB\xBFa;b\n"), b';');
    }

    #[test]
    fn a_field_is_quoted_only_when_it_needs_to_be() {
        let mut out = String::new();
        write_field(&mut out, "plain", ',');
        out.push('|');
        write_field(&mut out, "a,b", ',');
        out.push('|');
        write_field(&mut out, "say \"hi\"", ',');
        out.push('|');
        write_field(&mut out, "a;b", ',');

        assert_eq!(out, "plain|\"a,b\"|\"say \"\"hi\"\"\"|a;b");
    }
}
