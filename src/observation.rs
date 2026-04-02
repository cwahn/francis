use std::fmt;

use chrono::{DateTime, Utc};

/// What happened to a prediction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationKind {
    Staged,
    Observed,
    Failed,
}

impl fmt::Display for ObservationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObservationKind::Staged => write!(f, "Staged"),
            ObservationKind::Observed => write!(f, "Observed"),
            ObservationKind::Failed => write!(f, "FAILED"),
        }
    }
}

/// A single observation event in the audit trail.
#[derive(Debug, Clone)]
pub struct Observation {
    pub kind: ObservationKind,
    pub prediction: String,
    pub timestamp: DateTime<Utc>,
    pub log_line: Option<String>,
}

/// Complete audit trail for a successful run.
#[derive(Debug)]
pub struct Audit {
    pub observations: Vec<Observation>,
}

/// Describes a failed prediction with enough context for focused debugging.
#[derive(Debug)]
pub struct FailureReport {
    pub failed_prediction: String,
    pub pattern: String,
    pub search_start: DateTime<Utc>,
    pub search_end: DateTime<Utc>,
    pub audit: Audit,
}

/// Outcome of running a theory.
#[derive(Debug)]
pub enum RunResult {
    Pass(Audit),
    Fail(FailureReport),
}

impl fmt::Display for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for obs in &self.observations {
            let ts = obs.timestamp.format("%H:%M:%S%.3fZ");
            match &obs.log_line {
                Some(line) => {
                    let truncated = if line.len() > 120 {
                        format!("{}...", &line[..120])
                    } else {
                        line.clone()
                    };
                    writeln!(f, "  [{:<8}] {:<24} at {}  {}", obs.kind, obs.prediction, ts, truncated)?;
                }
                None => {
                    writeln!(f, "  [{:<8}] {:<24} at {}", obs.kind, obs.prediction, ts)?;
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for FailureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "✗ FAILED: {}", self.failed_prediction)?;
        writeln!(f, "  Pattern: {}", self.pattern)?;
        writeln!(f, "  Window:  [{}, {}]",
            self.search_start.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            self.search_end.format("%Y-%m-%dT%H:%M:%S%.3fZ"))?;
        writeln!(f)?;
        writeln!(f, "  Audit trail:")?;
        write!(f, "{}", self.audit)
    }
}

impl fmt::Display for RunResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunResult::Pass(audit) => {
                writeln!(f, "✓ PASS — all predictions observed")?;
                write!(f, "{}", audit)
            }
            RunResult::Fail(report) => write!(f, "{}", report),
        }
    }
}
