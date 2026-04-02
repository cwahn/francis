use std::path::PathBuf;
use std::process;

use chrono::{DateTime, Utc};
use clap::{Parser, ValueEnum};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use francis::hypothesis::RunConfig;
use francis::observation::RunResult;
use francis::runner;
use francis::validate;

/// Francis — Log-based hypothesis verifier.
///
/// Takes a theory (JSON file of predicted log events), polls Loki,
/// and verifies each prediction appears within its timeout window.
#[derive(Parser)]
#[command(name = "francis", version, about)]
struct Cli {
    /// Path to the theory JSON file.
    theory: PathBuf,

    /// Reference time (t0) for the root prediction.
    /// Accepts RFC 3339 timestamps or "now".
    #[arg(long, default_value = "now")]
    t0: String,

    /// Override the Loki URL from the theory file.
    #[arg(long)]
    loki_url: Option<String>,

    /// Override the base LogQL query from the theory file.
    #[arg(long)]
    base_query: Option<String>,

    /// Validate the theory without running it.
    #[arg(long)]
    dry_run: bool,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

fn parse_t0(s: &str) -> Result<DateTime<Utc>, String> {
    if s == "now" {
        return Ok(Utc::now());
    }
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("invalid t0 timestamp: {e}"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Load theory
    let contents = match std::fs::read_to_string(&cli.theory) {
        Err(e) => {
            error!(path = %cli.theory.display(), "failed to read theory file: {e}");
            process::exit(1);
        }
        Ok(c) => c,
    };

    let mut config: RunConfig = match serde_json::from_str(&contents) {
        Err(e) => {
            error!("failed to parse theory JSON: {e}");
            process::exit(1);
        }
        Ok(c) => c,
    };

    // Apply overrides
    if let Some(url) = cli.loki_url {
        config.source.url = url;
    }
    if let Some(bq) = cli.base_query {
        config.source.base_query = bq;
    }

    // Validate
    if let Err(errors) = validate::validate(&config.hypothesis) {
        error!("hypothesis validation failed:");
        for e in &errors {
            error!("  {e}");
        }
        process::exit(1);
    }
    info!("hypothesis validated");

    if cli.dry_run {
        info!("dry run — skipping execution");
        process::exit(0);
    }

    // Parse t0
    let t0 = match parse_t0(&cli.t0) {
        Ok(t) => t,
        Err(e) => {
            error!("{e}");
            process::exit(1);
        }
    };

    info!(%t0, "starting verification");

    // Run
    let result = runner::run(&config, t0).await;

    match &result {
        RunResult::Pass(_) => {
            match cli.output {
                OutputFormat::Text => println!("{result}"),
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap())
                }
            }
            process::exit(0);
        }
        RunResult::Fail(_) => {
            match cli.output {
                OutputFormat::Text => println!("{result}"),
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap())
                }
            }
            process::exit(1);
        }
    }
}
