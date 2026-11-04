//! Process-local logging; no background service is started.
use tracing_subscriber::EnvFilter;
/// Initialize console logging. RUST_LOG controls verbosity; default is info.
pub fn init_logging() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init()?;
    Ok(())
}
