use std::error::Error;
use std::fmt;

/// A value that failed validation. Names the field and the rule, never the value,
/// so it is safe to show to a caller even when the field is a password.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationError {
    field: &'static str,
    message: &'static str,
}

impl ValidationError {
    pub const fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }

    /// The name of the offending field.
    pub const fn field(&self) -> &'static str {
        self.field
    }

    /// What the field must satisfy.
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_names_the_field_and_the_rule() {
        let error = ValidationError::new("email", "is not a valid address");

        assert_eq!(error.field(), "email");
        assert_eq!(error.message(), "is not a valid address");
        assert_eq!(error.to_string(), "email: is not a valid address");
    }
}
