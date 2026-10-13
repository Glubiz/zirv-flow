//! The errors of the file source: static codes, so a run's log and the test-connection answer
//! never quote the file or its address.

use crate::sources::SourceError;

fn code(code: &'static str) -> SourceError {
    SourceError::Other(code.to_owned())
}

/// The file changed between two windows of one read.
pub(super) fn file_changed() -> SourceError {
    code("file_changed")
}

/// The server does not answer byte ranges and the file is too large to take whole.
pub(super) fn file_range_unsupported() -> SourceError {
    code("file_range_unsupported")
}

/// One record is over 1 MiB.
pub(super) fn record_too_large() -> SourceError {
    code("record_too_large")
}

pub(super) fn csv_invalid() -> SourceError {
    code("csv_invalid")
}

pub(super) fn json_invalid() -> SourceError {
    code("json_invalid")
}

/// A JSON file whose top level is not an array.
pub(super) fn json_not_an_array() -> SourceError {
    code("json_not_an_array")
}

/// Something other than whitespace follows the closing bracket of the array.
pub(super) fn json_trailing_data() -> SourceError {
    code("json_trailing_data")
}

/// The checkpoint is not one this source wrote.
pub(super) fn checkpoint_invalid() -> SourceError {
    code("checkpoint_invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_problem_is_a_static_code_without_a_value() {
        for (error, code) in [
            (file_changed(), "file_changed"),
            (file_range_unsupported(), "file_range_unsupported"),
            (record_too_large(), "record_too_large"),
            (csv_invalid(), "csv_invalid"),
            (json_invalid(), "json_invalid"),
            (json_not_an_array(), "json_not_an_array"),
            (json_trailing_data(), "json_trailing_data"),
            (checkpoint_invalid(), "checkpoint_invalid"),
        ] {
            assert_eq!(error, SourceError::Other(code.to_owned()));
            assert_eq!(error.to_string(), code);
        }
    }
}
