//! Fetch model pricing from provider APIs (currently OpenRouter).

use std::sync::Arc;

use eyre::Result;
use tracing::{info, warn};

use workforce_common::cost::CostTracker;
use workforce_common::persistence::StoredModelPricing;
use workforce_common::telemetry::TelemetryStore;

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Fetch pricing from OpenRouter and update the cost tracker.
///
/// Only re-fetches if the last fetch was >24 hours ago.
pub async fn fetch_openrouter_pricing(
    api_key: &str,
    store: &Arc<dyn TelemetryStore>,
    cost_tracker: &Arc<CostTracker>,
) -> Result<usize> {
    // Check staleness — skip if fetched recently
    if let Ok(entries) = store.list_model_pricing() {
        let now = chrono::Utc::now();
        let recent_fetch = entries.iter().any(|e| {
            e.source == "api_fetch"
                && chrono::DateTime::parse_from_rfc3339(&e.fetched_at)
                    .map(|dt| (now - dt.with_timezone(&chrono::Utc)).num_hours() < 24)
                    .unwrap_or(false)
        });
        if recent_fetch {
            info!("OpenRouter pricing fetched within 24h, skipping");
            return Ok(0);
        }
    }

    info!("Fetching model pricing from OpenRouter API");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client
        .get(OPENROUTER_MODELS_URL)
        .bearer_auth(api_key)
        .send()
        .await?;

    if !resp.status().is_success() {
        warn!(
            status = %resp.status(),
            "OpenRouter models API returned non-success status"
        );
        return Ok(0);
    }

    let json: serde_json::Value = resp.json().await?;
    let models = json["data"]
        .as_array()
        .ok_or_else(|| eyre::eyre!("OpenRouter response missing 'data' array"))?;

    let now = chrono::Utc::now().to_rfc3339();
    let mut count = 0;

    for model in models {
        let id = match model["id"].as_str() {
            Some(id) => id,
            None => continue,
        };

        let pricing = &model["pricing"];
        let prompt_str = match pricing["prompt"].as_str() {
            Some(s) => s,
            None => continue,
        };
        let completion_str = match pricing["completion"].as_str() {
            Some(s) => s,
            None => continue,
        };

        let prompt_per_token: f64 = match prompt_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let completion_per_token: f64 = match completion_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Convert per-token to per-million
        let input_per_million = prompt_per_token * 1_000_000.0;
        let output_per_million = completion_per_token * 1_000_000.0;

        // Skip free models (pricing = 0)
        if input_per_million == 0.0 && output_per_million == 0.0 {
            continue;
        }

        if let Err(e) = store.upsert_model_pricing(&StoredModelPricing {
            model: id.to_string(),
            provider: "openrouter".to_string(),
            input_per_million,
            output_per_million,
            source: "api_fetch".to_string(),
            fetched_at: now.clone(),
            updated_at: now.clone(),
        }) {
            warn!(model = id, error = %e, "Failed to store OpenRouter pricing");
            continue;
        }

        count += 1;
    }

    // Reload pricing into cost tracker from the updated store
    cost_tracker.load_pricing_from_store();

    info!(models_updated = count, "OpenRouter pricing fetch complete");

    Ok(count)
}
