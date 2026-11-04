fn main() -> Result<(), Box<dyn std::error::Error>> {
    suite_common::logging::init_logging()?;
    tracing::info!("{} foundation executable started", spreadsheet_engine::engine_name());
    tracing::info!("GUI framework selection is pending evaluation.");
    Ok(())
}
