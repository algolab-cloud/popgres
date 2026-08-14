//! Stable process exit codes for errors that automation needs to distinguish.

use std::fmt;

pub const GENERAL: i32 = 1;
pub const ALREADY_RUNNING: i32 = 2;
pub const PORT_BUSY: i32 = 3;
pub const NOT_RUNNING: i32 = 4;

#[derive(Debug)]
struct CodedError {
    exit_code: i32,
    message: String,
}

impl fmt::Display for CodedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

pub fn coded(exit_code: i32, message: impl Into<String>) -> anyhow::Error {
    CodedError {
        exit_code,
        message: message.into(),
    }
    .into()
}

pub fn exit_code(error: &anyhow::Error) -> i32 {
    error
        .downcast_ref::<CodedError>()
        .map_or(GENERAL, |error| error.exit_code)
}
