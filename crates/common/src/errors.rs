use eyre::Result;

/// Initialise color-eyre for nice error reporting.
pub fn setup_error_handling() -> Result<()> {
    color_eyre::install().map_err(|e| eyre::eyre!("Failed to install color-eyre: {}", e))?;
    Ok(())
}
