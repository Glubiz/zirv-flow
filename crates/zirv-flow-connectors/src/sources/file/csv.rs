//! A CSV window: records out of bytes that begin at a record boundary.

use serde_json::{Map, Value};
use zirv_flow_core::csv_file::{CsvError, CsvParser};
use zirv_flow_core::mapping::{Header, HeaderError};

use super::{Parsed, error};
use crate::sources::SourceError;

/// Reads one window of a CSV file. The first window of the file reads its header from the
/// first record; later windows are given it.
pub(super) struct CsvWindow {
    parser: CsvParser,
    delimiter: u8,
    header: Option<Vec<String>>,
    /// The record the next data record is, counting from 1.
    ordinal: u64,
}

impl CsvWindow {
    /// A window at the start of the file, which holds the header.
    pub(super) fn at_start(delimiter: u8) -> Self {
        Self {
            parser: CsvParser::new(delimiter),
            delimiter,
            header: None,
            ordinal: 0,
        }
    }

    /// A window that begins at a record boundary; `records` data records came before it.
    pub(super) fn resumed(delimiter: u8, header: Vec<String>, records: u64) -> Self {
        Self {
            delimiter,
            // The header is row 1, so the data record after `records` of them is row
            // `records + 2`.
            parser: CsvParser::starting_at_row(delimiter, records + 1),
            header: Some(header),
            ordinal: records,
        }
    }

    pub(super) fn delimiter(&self) -> u8 {
        self.delimiter
    }

    pub(super) fn header(&self) -> Option<&[String]> {
        self.header.as_deref()
    }

    /// The data records handed over so far in the file.
    pub(super) fn ordinal(&self) -> u64 {
        self.ordinal
    }

    pub(super) fn push(&mut self, slice: &[u8], out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        let mut records = Vec::new();
        self.parser
            .push(slice, &mut |_, record| records.push(record))
            .map_err(csv_error)?;
        self.take(records, out)
    }

    pub(super) fn finish(&mut self, out: &mut Vec<Parsed>) -> Result<(), SourceError> {
        let mut records = Vec::new();
        self.parser
            .finish(&mut |_, record| records.push(record))
            .map_err(csv_error)?;
        self.take(records, out)
    }

    /// How many bytes of the window are done with: up to the end of the last line.
    pub(super) fn consumed(&self) -> u64 {
        self.parser.boundary()
    }

    fn take(
        &mut self,
        records: Vec<Vec<String>>,
        out: &mut Vec<Parsed>,
    ) -> Result<(), SourceError> {
        let end = self.parser.boundary();
        for cells in records {
            let Some(header) = &self.header else {
                let names = Header::new(cells).map_err(header_error)?;
                self.header = Some(names.names().to_vec());
                continue;
            };
            self.ordinal += 1;
            let mut object = Map::with_capacity(header.len());
            for (name, cell) in header
                .iter()
                .zip(cells.into_iter().chain(std::iter::repeat(String::new())))
            {
                object.insert(name.clone(), Value::String(cell));
            }
            out.push(Parsed {
                record: object,
                end,
                ordinal: self.ordinal,
            });
        }
        Ok(())
    }
}

fn csv_error(error: CsvError) -> SourceError {
    match error {
        CsvError::RecordTooLarge => error::record_too_large(),
        CsvError::NotUtf8 | CsvError::TooManyFields | CsvError::UnterminatedQuote => {
            error::csv_invalid()
        }
    }
}

fn header_error(error: HeaderError) -> SourceError {
    SourceError::Other(error.code().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: &[u8], piece: usize) -> Result<(Vec<Parsed>, Option<Vec<String>>), SourceError> {
        let mut window = CsvWindow::at_start(b',');
        let mut out = Vec::new();
        for slice in bytes.chunks(piece) {
            window.push(slice, &mut out)?;
        }
        window.finish(&mut out)?;
        Ok((out, window.header().map(<[String]>::to_vec)))
    }

    #[test]
    fn the_first_record_is_the_header_and_the_rest_are_objects_keyed_by_it() {
        let (records, header) = run(b"id,name\r\n1,Ada\r\n2,\"B, \"\"x\"\"\"\r\n3\r\n", 5).unwrap();

        assert_eq!(header.unwrap(), ["id", "name"]);
        let values: Vec<_> = records
            .iter()
            .map(|r| Value::Object(r.record.clone()))
            .collect();
        assert_eq!(values[0], serde_json::json!({"id": "1", "name": "Ada"}));
        assert_eq!(
            values[1],
            serde_json::json!({"id": "2", "name": "B, \"x\""})
        );
        assert_eq!(values[2], serde_json::json!({"id": "3", "name": ""}));
        let ordinals: Vec<_> = records.iter().map(|r| r.ordinal).collect();
        assert_eq!(ordinals, [1, 2, 3]);
    }

    #[test]
    fn a_blank_or_duplicate_header_name_is_refused_with_its_code() {
        assert_eq!(
            run(b"a,,c\n1,2,3\n", 100).err(),
            Some(SourceError::Other("blank_header".into()))
        );
        assert_eq!(
            run(b"a,a\n1,2\n", 100).err(),
            Some(SourceError::Other("duplicate_header".into()))
        );
    }

    #[test]
    fn invalid_utf8_and_an_unclosed_quote_never_quote_the_file() {
        let bad = run(b"a\n\xFF\xFE\n", 100).err().unwrap();
        let open = run(b"a\n\"never closed", 100).err().unwrap();

        assert_eq!(bad, SourceError::Other("csv_invalid".into()));
        assert_eq!(open, SourceError::Other("csv_invalid".into()));
    }

    #[test]
    fn a_record_over_a_mebibyte_is_too_large() {
        let mut bytes = b"a\n\"".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', (1 << 20) + 10));
        bytes.extend_from_slice(b"\"\n");

        assert_eq!(
            run(&bytes, 64 * 1024).err(),
            Some(SourceError::Other("record_too_large".into()))
        );
    }

    #[test]
    fn a_resumed_window_reads_records_without_a_header_and_continues_the_count() {
        let mut window = CsvWindow::resumed(b',', vec!["a".into(), "b".into()], 7);
        let mut out = Vec::new();

        window.push(b"1,2\n3,4\n", &mut out).unwrap();
        window.finish(&mut out).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ordinal, 8);
        assert_eq!(out[1].ordinal, 9);
        assert_eq!(out[1].end, 8);
        assert_eq!(window.ordinal(), 9);
    }
}
