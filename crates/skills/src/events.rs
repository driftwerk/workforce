use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An event published by an event source into NATS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unique event ID.
    pub id: Uuid,
    /// Event type (e.g., "email.incoming", "git.pr.opened").
    pub event_type: String,
    /// Which source produced this event.
    pub source: String,
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
    /// Structured event data.
    pub data: serde_json::Value,
}

impl Event {
    pub fn new(
        event_type: impl Into<String>,
        source: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            event_type: event_type.into(),
            source: source.into(),
            timestamp: Utc::now(),
            data,
        }
    }
}

/// Configuration for an event source instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSourceConfig {
    /// Source name (unique identifier for this instance).
    pub name: String,
    /// Adapter type (e.g., "gmail_webhook", "github_webhook", "cron").
    pub adapter_type: String,
    /// Adapter-specific configuration.
    pub config: serde_json::Value,
    /// NATS subject prefix to publish events to.
    pub publish_to: String,
    /// Whether this source is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Trait for event source adapters.
///
/// Each adapter type implements this to watch an external system
/// and emit events into the workforce.
#[async_trait]
pub trait EventAdapter: Send + Sync {
    /// The adapter type name (e.g., "gmail_webhook").
    fn adapter_type(&self) -> &str;

    /// Validate the configuration for this adapter.
    fn validate_config(&self, config: &serde_json::Value) -> eyre::Result<()>;

    /// Start watching and emit events via the callback.
    ///
    /// This should run until the cancellation token fires.
    /// Events are pushed via `emit`.
    async fn start(
        &self,
        config: serde_json::Value,
        emit: Box<dyn Fn(Event) + Send + Sync>,
    ) -> eyre::Result<()>;

    /// Stop watching.
    async fn stop(&self) -> eyre::Result<()>;
}

// ─── Built-in Adapters ───

/// Cron/schedule adapter — fires events on a schedule.
pub struct CronAdapter;

impl CronAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CronAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventAdapter for CronAdapter {
    fn adapter_type(&self) -> &str {
        "cron"
    }

    fn validate_config(&self, config: &serde_json::Value) -> eyre::Result<()> {
        config["schedule"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Cron adapter requires 'schedule' string in config"))?;
        Ok(())
    }

    async fn start(
        &self,
        config: serde_json::Value,
        emit: Box<dyn Fn(Event) + Send + Sync>,
    ) -> eyre::Result<()> {
        let schedule = config["schedule"]
            .as_str()
            .unwrap_or("*/5 * * * *")
            .to_string();
        let name = config["name"].as_str().unwrap_or("cron-job").to_string();

        // Emit an initial event to indicate the source started
        emit(Event::new(
            "schedule.started",
            format!("cron:{}", name),
            serde_json::json!({
                "schedule": schedule,
                "name": name,
            }),
        ));

        // In production this would run a cron loop.
        // For now, just emit one event and return.
        emit(Event::new(
            "schedule.fired",
            format!("cron:{}", name),
            serde_json::json!({
                "schedule": schedule,
                "name": name,
                "fired_at": Utc::now().to_rfc3339(),
            }),
        ));

        Ok(())
    }

    async fn stop(&self) -> eyre::Result<()> {
        Ok(())
    }
}

/// Webhook adapter — receives HTTP webhooks and converts to events.
pub struct WebhookAdapter {
    adapter_name: String,
    event_type_prefix: String,
}

impl WebhookAdapter {
    pub fn new(adapter_name: impl Into<String>, event_type_prefix: impl Into<String>) -> Self {
        Self {
            adapter_name: adapter_name.into(),
            event_type_prefix: event_type_prefix.into(),
        }
    }

    /// Create a Gmail webhook adapter.
    pub fn gmail() -> Self {
        Self::new("gmail_webhook", "email")
    }

    /// Create a GitHub webhook adapter.
    pub fn github() -> Self {
        Self::new("github_webhook", "git")
    }

    /// Process an incoming webhook payload and return an event.
    pub fn process_webhook(&self, payload: serde_json::Value) -> Event {
        let event_type = self.detect_event_type(&payload);
        Event::new(event_type, &self.adapter_name, payload)
    }

    fn detect_event_type(&self, payload: &serde_json::Value) -> String {
        // Gmail webhook
        if self.adapter_name == "gmail_webhook" {
            return format!("{}.incoming", self.event_type_prefix);
        }

        // GitHub webhook — detect from action/event fields
        if self.adapter_name == "github_webhook" {
            if let Some(action) = payload.get("action").and_then(|a| a.as_str()) {
                if payload.get("pull_request").is_some() {
                    return format!("{}.pr.{}", self.event_type_prefix, action);
                }
                if payload.get("issue").is_some() {
                    return format!("{}.issue.{}", self.event_type_prefix, action);
                }
                if payload.get("release").is_some() {
                    return format!("{}.release.{}", self.event_type_prefix, action);
                }
            }
            if payload.get("ref").is_some() && payload.get("commits").is_some() {
                return format!("{}.push", self.event_type_prefix);
            }
        }

        format!("{}.unknown", self.event_type_prefix)
    }
}

#[async_trait]
impl EventAdapter for WebhookAdapter {
    fn adapter_type(&self) -> &str {
        &self.adapter_name
    }

    fn validate_config(&self, _config: &serde_json::Value) -> eyre::Result<()> {
        Ok(())
    }

    async fn start(
        &self,
        _config: serde_json::Value,
        _emit: Box<dyn Fn(Event) + Send + Sync>,
    ) -> eyre::Result<()> {
        // In production, this would start an HTTP server to receive webhooks.
        // The actual webhook handling is done via process_webhook().
        Ok(())
    }

    async fn stop(&self) -> eyre::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_event() {
        let event = Event::new(
            "email.incoming",
            "gmail",
            serde_json::json!({"from": "alice@example.com", "subject": "Hello"}),
        );
        assert_eq!(event.event_type, "email.incoming");
        assert_eq!(event.source, "gmail");
    }

    #[test]
    fn cron_config_validation() {
        let adapter = CronAdapter::new();
        let valid = serde_json::json!({"schedule": "*/5 * * * *"});
        assert!(adapter.validate_config(&valid).is_ok());

        let invalid = serde_json::json!({"interval": 60});
        assert!(adapter.validate_config(&invalid).is_err());
    }

    #[tokio::test]
    async fn cron_emits_events() {
        let adapter = CronAdapter::new();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();

        adapter
            .start(
                serde_json::json!({"schedule": "*/5 * * * *", "name": "test-job"}),
                Box::new(move |e| {
                    events_clone.lock().unwrap().push(e);
                }),
            )
            .await
            .unwrap();

        let collected = events.lock().unwrap();
        assert_eq!(collected.len(), 2); // started + fired
        assert_eq!(collected[0].event_type, "schedule.started");
        assert_eq!(collected[1].event_type, "schedule.fired");
    }

    #[test]
    fn github_webhook_pr_opened() {
        let adapter = WebhookAdapter::github();
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "title": "Add login feature"
            }
        });

        let event = adapter.process_webhook(payload);
        assert_eq!(event.event_type, "git.pr.opened");
    }

    #[test]
    fn github_webhook_push() {
        let adapter = WebhookAdapter::github();
        let payload = serde_json::json!({
            "ref": "refs/heads/main",
            "commits": [{"message": "fix bug"}]
        });

        let event = adapter.process_webhook(payload);
        assert_eq!(event.event_type, "git.push");
    }

    #[test]
    fn github_webhook_issue() {
        let adapter = WebhookAdapter::github();
        let payload = serde_json::json!({
            "action": "created",
            "issue": {"number": 10, "title": "Bug report"}
        });

        let event = adapter.process_webhook(payload);
        assert_eq!(event.event_type, "git.issue.created");
    }

    #[test]
    fn gmail_webhook() {
        let adapter = WebhookAdapter::gmail();
        let payload = serde_json::json!({
            "from": "alice@example.com",
            "subject": "Meeting tomorrow"
        });

        let event = adapter.process_webhook(payload);
        assert_eq!(event.event_type, "email.incoming");
    }

    #[test]
    fn event_serialization() {
        let event = Event::new(
            "test.event",
            "test-source",
            serde_json::json!({"key": "value"}),
        );
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.event_type, "test.event");
        assert_eq!(deserialized.id, event.id);
    }
}
