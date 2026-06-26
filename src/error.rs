//! Error type carrying the process exit code, mirroring the reference's UsageError vs other split.

use std::fmt;

#[derive(Debug)]
pub enum AppError {
    /// Bad usage: prints `try 'wake --help'` and exits 2.
    Usage(String),
    /// Any other failure: exits 1.
    Fail(String),
}

impl AppError {
    pub fn usage(msg: impl Into<String>) -> Self {
        AppError::Usage(msg.into())
    }
    pub fn fail(msg: impl Into<String>) -> Self {
        AppError::Fail(msg.into())
    }
    pub fn message(&self) -> &str {
        match self {
            AppError::Usage(m) | AppError::Fail(m) => m,
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::Usage(_) => 2,
            AppError::Fail(_) => 1,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Fail(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
