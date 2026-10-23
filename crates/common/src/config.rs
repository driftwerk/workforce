use eyre::Result;
use serde::Deserialize;

/// Load configuration from layered sources.
///
/// Priority (highest wins):
/// 1. Environment variables (prefixed with `WORKFORCE_`)
/// 2. Local config file (`./workforce.toml`)
/// 3. User config file (`~/.workforce/config.toml`)
/// 4. Defaults
pub fn load_config<T: for<'de> Deserialize<'de>>() -> Result<T> {
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

    let settings = config::Config::builder()
        // User-level config
        .add_source(
            config::File::with_name(&format!("{home_dir}/.workforce/config")).required(false),
        )
        // Project-level config
        .add_source(config::File::with_name("workforce").required(false))
        // Environment variables: WORKFORCE_NATS__URL -> nats.url
        .add_source(
            config::Environment::with_prefix("WORKFORCE")
                .separator("__")
                .try_parsing(true),
        )
        .build()
        .map_err(|e| eyre::eyre!("Failed to build configuration: {}", e))?;

    settings
        .try_deserialize()
        .map_err(|e| eyre::eyre!("Failed to deserialise configuration: {}", e))
}
