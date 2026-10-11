//! The little CSV the suppression import and export need: RFC 4180 records with quoted
//! fields, `""` as an escaped quote, and LF or CRLF line ends.

/// One record and the line of the file it starts on, the first line being 1.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Record {
    pub(super) line: usize,
    pub(super) fields: Vec<String>,
}

/// Splits `text` into records of fields. A byte order mark at the start is ignored, blank
/// lines are skipped, and a quote that never closes ends the last field at the end of the
/// text. Lines are counted by LF, by CRLF once, and by a CR on its own; a line break inside
/// quotes counts too.
pub(super) fn parse(text: &str) -> Vec<Record> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut in_record = false;
    let mut line = 1;
    let mut start = 1;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let breaks = c == '\n' || (c == '\r' && chars.peek() != Some(&'\n'));
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            if breaks {
                line += 1;
            }
            continue;
        }
        if breaks {
            if in_record {
                record.push(std::mem::take(&mut field));
                records.push(Record {
                    line: start,
                    fields: std::mem::take(&mut record),
                });
            }
            in_record = false;
            line += 1;
            continue;
        }
        if c == '\r' {
            continue;
        }
        if !in_record {
            in_record = true;
            start = line;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => record.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    if in_record {
        record.push(field);
        records.push(Record {
            line: start,
            fields: record,
        });
    }
    records
}

/// One field of an exported record: quoted when it holds a comma, a quote or a line break.
pub(super) fn field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<Vec<String>> {
        parse(text)
            .into_iter()
            .map(|record| record.fields)
            .collect()
    }

    fn lines(text: &str) -> Vec<usize> {
        parse(text).into_iter().map(|record| record.line).collect()
    }

    fn record(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|field| (*field).to_owned()).collect()
    }

    #[test]
    fn records_split_on_commas_and_either_line_end() {
        assert_eq!(
            rows("kind,value\nemail,a@x.com\r\nphone,+4512345678"),
            [
                record(&["kind", "value"]),
                record(&["email", "a@x.com"]),
                record(&["phone", "+4512345678"])
            ]
        );
    }

    #[test]
    fn quoted_fields_keep_commas_quotes_and_line_breaks() {
        assert_eq!(
            rows("a,\"b,c\",\"say \"\"hi\"\"\",\"x\ny\"\n"),
            [record(&["a", "b,c", "say \"hi\"", "x\ny"])]
        );
    }

    #[test]
    fn blank_lines_and_a_byte_order_mark_are_ignored_and_empty_fields_kept() {
        assert_eq!(
            rows("\u{feff}kind,value,note\n\n\nemail,a@x.com,\n,,\n"),
            [
                record(&["kind", "value", "note"]),
                record(&["email", "a@x.com", ""]),
                record(&["", "", ""])
            ]
        );
    }

    #[test]
    fn an_unclosed_quote_ends_at_the_end_of_the_text() {
        assert_eq!(rows("a,\"b\nc"), [record(&["a", "b\nc"])]);
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_record_reports_the_line_it_starts_on() {
        assert_eq!(lines("a\nb\nc"), [1, 2, 3]);
        assert_eq!(lines("a\n\n\nb"), [1, 4]);
        assert_eq!(lines("\u{feff}a\r\nb\r\n\r\nc"), [1, 2, 4]);
        assert_eq!(lines("a\rb\r\rc"), [1, 2, 4]);
        assert_eq!(lines("a,\"x\ny\nz\"\nb"), [1, 4]);
        assert_eq!(lines("a,\"x\r\ny\"\nb\r"), [1, 3]);
        assert_eq!(lines("a,\"x\ry\"\nb"), [1, 3]);
    }

    #[test]
    fn an_exported_field_is_quoted_only_when_it_needs_to_be() {
        assert_eq!(field("plain"), "plain");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(
            rows(&format!("{},x", field("a\"b,c"))),
            [record(&["a\"b,c", "x"])]
        );
    }
}
