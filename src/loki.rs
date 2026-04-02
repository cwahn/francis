use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum LokiError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Loki returned status {0}: {1}")]
    Status(String, String),
    #[error("unexpected response shape")]
    BadResponse,
}

/// A single log entry returned from Loki.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub line: String,
}

/// Minimal Loki query_range response structure.
#[derive(Deserialize)]
struct QueryRangeResponse {
    status: String,
    data: Option<QueryRangeData>,
}

#[derive(Deserialize)]
struct QueryRangeData {
    result: Vec<Stream>,
}

#[derive(Deserialize)]
struct Stream {
    values: Vec<(String, String)>,
}

pub struct LokiClient {
    client: Client,
    base_url: String,
}

impl LokiClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    /// Query Loki for the earliest log line matching `query` in `[start, end]`.
    ///
    /// `query` is a complete LogQL selector+pipeline, e.g.
    /// `{service_name="organon-core"} |= "bi_count=1"`.
    ///
    /// Returns the first matching entry (direction=forward, limit=1), or `None`.
    pub async fn query_first(
        &self,
        query: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Option<LogEntry>, LokiError> {
        let start_ns = start.timestamp_nanos_opt().unwrap_or(0).to_string();
        let end_ns = end.timestamp_nanos_opt().unwrap_or(0).to_string();

        debug!(query, %start, %end, "loki query_range");

        let resp = self
            .client
            .get(format!("{}/loki/api/v1/query_range", self.base_url))
            .query(&[
                ("query", query),
                ("start", &start_ns),
                ("end", &end_ns),
                ("limit", "1"),
                ("direction", "forward"),
            ])
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?;

        let body: QueryRangeResponse = resp.json().await?;

        if body.status != "success" {
            return Err(LokiError::Status(
                body.status,
                "query did not succeed".into(),
            ));
        }

        let data = body.data.ok_or(LokiError::BadResponse)?;

        // Find the earliest entry across all streams.
        let entry = data
            .result
            .into_iter()
            .flat_map(|s| s.values.into_iter())
            .filter_map(|(ts_str, line)| {
                let nanos: i64 = ts_str.parse().ok()?;
                let ts = DateTime::from_timestamp_nanos(nanos);
                Some(LogEntry { timestamp: ts, line })
            })
            .min_by_key(|e| e.timestamp);

        Ok(entry)
    }
}
