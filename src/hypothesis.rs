use serde::Deserialize;

/// A binding name used to reference a prediction's observation.
pub type Binding = String;

/// Millisecond duration for timeouts.
pub type TimeoutMs = u64;

/// Top-level run configuration loaded from JSON.
#[derive(Debug, Deserialize)]
pub struct RunConfig {
    pub source: LokiSource,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_ingestion_slack")]
    pub ingestion_slack_ms: u64,
    pub hypothesis: PredictionDef,
}

fn default_poll_interval() -> u64 {
    500
}

fn default_ingestion_slack() -> u64 {
    5000
}

/// Loki connection parameters.
#[derive(Debug, Deserialize)]
pub struct LokiSource {
    pub url: String,
    pub base_query: String,
}

/// A prediction about an expected log event (or group thereof).
#[derive(Debug, Deserialize)]
pub enum PredictionDef {
    Unit(UnitPrediction),
    All(GroupPrediction),
    Any(GroupPrediction),
}

/// A single expected log line.
#[derive(Debug, Deserialize)]
pub struct UnitPrediction {
    pub binding: Option<Binding>,
    /// LogQL line filter pipeline, e.g. `|= "service registered"`.
    pub pattern: String,
    /// Reference to a prior prediction's binding. `None` means "after previous sibling".
    pub after: Option<Binding>,
    pub timeout_ms: TimeoutMs,
}

/// A group of predictions (All = all must be observed, Any = at least one).
/// Ordering within a group is NOT positional — it comes only from `after` references.
#[derive(Debug, Deserialize)]
pub struct GroupPrediction {
    pub binding: Option<Binding>,
    /// Reference to a prior prediction's binding. `None` means "starts at parent's expected time".
    pub after: Option<Binding>,
    pub predictions: Vec<PredictionDef>,
}

impl PredictionDef {
    pub fn binding(&self) -> Option<&str> {
        match self {
            PredictionDef::Unit(u) => u.binding.as_deref(),
            PredictionDef::All(g) | PredictionDef::Any(g) => g.binding.as_deref(),
        }
    }

    pub fn after(&self) -> Option<&str> {
        match self {
            PredictionDef::Unit(u) => u.after.as_deref(),
            PredictionDef::All(g) | PredictionDef::Any(g) => g.after.as_deref(),
        }
    }
}
