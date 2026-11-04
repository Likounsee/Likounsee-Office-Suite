use thiserror::Error;
#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("Operation is not implemented yet: {0}")]
    NotImplemented(String),
}
pub type AppResult<T> = Result<T, AppError>;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn io_errors_are_preserved() {
        let error = AppError::from(std::io::Error::other("disk unavailable"));
        assert!(matches!(error, AppError::Io(_)));
        assert!(error.to_string().contains("disk unavailable"));
    }
}
