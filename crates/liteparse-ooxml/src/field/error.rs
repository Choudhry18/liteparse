use std::fmt;

/// Errors that can occur when parsing a field instruction string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldParseError {
    /// The instruction string is empty or contains only whitespace.
    Empty,
    /// A quoted string was not terminated.
    UnterminatedString { position: usize },
    /// A switch character (`\x`) was found but the letter is missing or invalid.
    InvalidSwitch { position: usize, found: String },
    /// A required argument is missing (e.g., REF without a bookmark name).
    MissingArgument {
        field_type: String,
        argument: String,
    },
    /// A numeric value could not be parsed (e.g., SYMBOL with non-numeric code).
    InvalidNumber { value: String, reason: String },
    /// An IF field has an invalid or missing comparison operator.
    InvalidOperator { found: String },
    /// A switch value was expected but not found.
    MissingSwitchValue { switch: String },
}

impl fmt::Display for FieldParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty field instruction"),
            Self::UnterminatedString { position } => {
                write!(f, "unterminated string at position {position}")
            }
            Self::InvalidSwitch { position, found } => {
                write!(f, "invalid switch '{found}' at position {position}")
            }
            Self::MissingArgument {
                field_type,
                argument,
            } => {
                write!(
                    f,
                    "{field_type} field missing required argument: {argument}"
                )
            }
            Self::InvalidNumber { value, reason } => {
                write!(f, "invalid number '{value}': {reason}")
            }
            Self::InvalidOperator { found } => {
                write!(f, "invalid comparison operator: '{found}'")
            }
            Self::MissingSwitchValue { switch } => {
                write!(f, "switch '{switch}' requires a value")
            }
        }
    }
}

impl std::error::Error for FieldParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_the_salient_detail() {
        assert_eq!(
            FieldParseError::Empty.to_string(),
            "empty field instruction"
        );
        assert_eq!(
            FieldParseError::MissingSwitchValue {
                switch: "\\o".into()
            }
            .to_string(),
            "switch '\\o' requires a value"
        );
        assert_eq!(
            FieldParseError::MissingArgument {
                field_type: "REF".into(),
                argument: "bookmark".into(),
            }
            .to_string(),
            "REF field missing required argument: bookmark"
        );
        assert_eq!(
            FieldParseError::UnterminatedString { position: 7 }.to_string(),
            "unterminated string at position 7"
        );
        assert_eq!(
            FieldParseError::InvalidOperator { found: "=?".into() }.to_string(),
            "invalid comparison operator: '=?'"
        );
    }
}
