//! Error type for descriptor parsing.

use std::fmt;

/// A class-specific descriptor could not be parsed or failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// The descriptor is truncated, has the wrong type/subtype, or holds a
    /// value the UAC specification does not allow. The string says which.
    Invalid(String),
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid descriptor: {message}"),
        }
    }
}

impl std::error::Error for DescriptorError {}
