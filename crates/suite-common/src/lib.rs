//! Shared, UI-independent foundations.
pub mod autosave;
pub mod error;
pub mod logging;
pub mod recovery;
pub mod storage;

pub use error::{AppError, AppResult};
