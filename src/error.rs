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

pub fn combine_cleanup<T>(primary: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(AppError::fail(format!(
            "{primary}; cleanup failed: {cleanup}"
        ))),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_combination_covers_all_result_pairs() {
        assert_eq!(combine_cleanup(Ok(7), Ok(())).unwrap(), 7);
        assert_eq!(
            combine_cleanup::<()>(Err(AppError::fail("primary")), Ok(()))
                .unwrap_err()
                .message(),
            "primary"
        );
        assert_eq!(
            combine_cleanup(Ok(7), Err(AppError::fail("cleanup")))
                .unwrap_err()
                .message(),
            "cleanup"
        );
        assert_eq!(
            combine_cleanup::<()>(
                Err(AppError::fail("primary")),
                Err(AppError::fail("cleanup")),
            )
            .unwrap_err()
            .message(),
            "primary; cleanup failed: cleanup"
        );
    }
}
