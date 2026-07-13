use std::fmt;

pub const INHIBITOR_STARTUP_EXIT_CODE: i32 = 3;

#[derive(Debug)]
pub enum AppError {
    Usage(String),
    Fail(String),
    InhibitorStartup(String),
}

impl AppError {
    pub fn usage(msg: impl Into<String>) -> Self {
        AppError::Usage(msg.into())
    }
    pub fn fail(msg: impl Into<String>) -> Self {
        AppError::Fail(msg.into())
    }
    pub fn inhibitor_startup(msg: impl Into<String>) -> Self {
        AppError::InhibitorStartup(msg.into())
    }
    pub fn message(&self) -> &str {
        match self {
            AppError::Usage(m) | AppError::Fail(m) | AppError::InhibitorStartup(m) => m,
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::Usage(_) => 2,
            AppError::Fail(_) => 1,
            AppError::InhibitorStartup(_) => INHIBITOR_STARTUP_EXIT_CODE,
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
