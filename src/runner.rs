use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use tracing::{debug, error, info, trace, warn};

use crate::loki::LokiClient;
use crate::observation::{Audit, FailureReport, Observation, ObservationKind, RunResult};
use crate::hypothesis::{Binding, PredictionDef, RunConfig};

// ---------------------------------------------------------------------------
// Internal node state
// ---------------------------------------------------------------------------

/// Flattened representation of a single prediction node for the state machine.
#[derive(Debug)]
struct Node {
    /// Index in the nodes vec.
    id: usize,
    /// Display name (binding or auto-generated).
    name: String,
    kind: NodeKind,
    state: NodeState,
    /// Index of parent node (None for root).
    #[allow(dead_code)]
    parent: Option<usize>,
}

#[derive(Debug)]
enum NodeKind {
    Unit {
        pattern: String,
        /// Explicit `after` binding, or None (use parent's expected time).
        after: Option<Binding>,
        timeout_ms: u64,
    },
    All {
        after: Option<Binding>,
        children: Vec<usize>,
    },
    Any {
        after: Option<Binding>,
        children: Vec<usize>,
    },
}

#[derive(Debug, Clone)]
enum NodeState {
    Pending,
    Expecting { at: DateTime<Utc> },
    Observed { at: DateTime<Utc>, line: Option<String> },
    Failed { expected_at: DateTime<Utc> },
}

impl NodeState {
    fn is_terminal(&self) -> bool {
        matches!(self, NodeState::Observed { .. } | NodeState::Failed { .. })
    }
}

// ---------------------------------------------------------------------------
// Capture helpers
// ---------------------------------------------------------------------------

/// Replace `${name}` placeholders in `pattern` with values from `captures`.
fn apply_captures(pattern: &str, captures: &HashMap<String, String>) -> String {
    let mut result = pattern.to_owned();
    for (name, value) in captures {
        result = result.replace(&format!("${{{}}}", name), value);
    }
    result
}

/// Run every `| regexp "..."` stage in `pattern` against `line` and return
/// any named capture groups found.
fn extract_regexp_captures(pattern: &str, line: &str) -> HashMap<String, String> {
    static STAGE_RE: OnceLock<Regex> = OnceLock::new();
    let stage_re = STAGE_RE.get_or_init(|| {
        Regex::new(r#"\|\s*regexp\s*"((?:[^"\\]|\\.)*)""#).unwrap()
    });

    let mut out = HashMap::new();
    for caps in stage_re.captures_iter(pattern) {
        let raw = caps[1].replace("\\\"", "\"");
        if let Ok(re) = Regex::new(&raw) {
            if let Some(m) = re.captures(line) {
                for name in re.capture_names().flatten() {
                    if let Some(v) = m.name(name) {
                        out.insert(name.to_owned(), v.as_str().to_owned());
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Flatten the tree into a vec
// ---------------------------------------------------------------------------

fn flatten(def: &PredictionDef, parent: Option<usize>, nodes: &mut Vec<Node>, anon_counter: &mut usize) {
    let id = nodes.len();
    let name = def
        .binding()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            *anon_counter += 1;
            format!("_anon_{}", anon_counter)
        });

    match def {
        PredictionDef::Unit(u) => {
            nodes.push(Node {
                id,
                name,
                kind: NodeKind::Unit {
                    pattern: u.pattern.clone(),
                    after: u.after.clone(),
                    timeout_ms: u.timeout_ms,
                },
                state: NodeState::Pending,
                parent,
            });
        }
        PredictionDef::All(g) => {
            nodes.push(Node {
                id,
                name,
                kind: NodeKind::All { after: g.after.clone(), children: Vec::new() },
                state: NodeState::Pending,
                parent,
            });
            let mut child_ids = Vec::with_capacity(g.predictions.len());
            for child_def in &g.predictions {
                let child_id = nodes.len();
                child_ids.push(child_id);
                flatten(child_def, Some(id), nodes, anon_counter);
            }
            if let NodeKind::All { children, .. } = &mut nodes[id].kind {
                *children = child_ids;
            }
        }
        PredictionDef::Any(g) => {
            nodes.push(Node {
                id,
                name,
                kind: NodeKind::Any { after: g.after.clone(), children: Vec::new() },
                state: NodeState::Pending,
                parent,
            });
            let mut child_ids = Vec::with_capacity(g.predictions.len());
            for child_def in &g.predictions {
                let child_id = nodes.len();
                child_ids.push(child_id);
                flatten(child_def, Some(id), nodes, anon_counter);
            }
            if let NodeKind::Any { children, .. } = &mut nodes[id].kind {
                *children = child_ids;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

pub async fn run(config: &RunConfig, t0: DateTime<Utc>) -> RunResult {
    let client = LokiClient::new(&config.source.url);
    let base_query = &config.source.base_query;
    let poll_interval = std::time::Duration::from_millis(config.poll_interval_ms);
    let ingestion_slack = Duration::milliseconds(config.ingestion_slack_ms as i64);

    // Flatten tree
    let mut nodes = Vec::new();
    let mut anon_counter = 0usize;
    flatten(&config.hypothesis, None, &mut nodes, &mut anon_counter);

    // Build name → id index
    let name_to_id: HashMap<String, usize> = nodes
        .iter()
        .map(|n| (n.name.clone(), n.id))
        .collect();

    // Observations audit trail
    let mut observations: Vec<Observation> = Vec::new();

    // Named captures accumulated across predictions
    let mut capture_store: HashMap<String, String> = HashMap::new();

    // Stage root
    expect_node(&mut nodes, 0, t0, &mut observations);

    loop {
        tokio::time::sleep(poll_interval).await;
        let now = Utc::now();

        // Expecting pass: propagate expecting pass to children
        expecting_pass(&mut nodes, t0, &name_to_id, &mut observations);

        // Query pass: poll Loki for each expecting Unit
        query_pass(&mut nodes, &client, base_query, now, ingestion_slack, &mut capture_store, &mut observations).await;

        // Propagation pass: propagate child results to parents
        propagation_pass(&mut nodes, &mut observations);

        // Check root
        let root = &nodes[0];
        match &root.state {
            NodeState::Failed { .. } => {
                // Find the first failed Unit for the report
                let (failed_name, pattern, search_start, search_end) =
                    find_failed_unit(&nodes, &name_to_id);
                warn!(prediction = %failed_name, "hypothesis falsified");
                return RunResult::Fail(FailureReport {
                    failed_prediction: failed_name,
                    pattern,
                    search_start,
                    search_end,
                    audit: Audit { observations },
                });
            }
            NodeState::Observed { .. } => {
                info!("hypothesis verified — all predictions observed");
                return RunResult::Pass(Audit { observations });
            }
            _ => {}
        }
    }
}

fn expect_node(
    nodes: &mut [Node],
    id: usize,
    at: DateTime<Utc>,
    observations: &mut Vec<Observation>,
) {
    if !matches!(nodes[id].state, NodeState::Pending) {
        return;
    }
    trace!(prediction = %nodes[id].name, "observing prediction");
    nodes[id].state = NodeState::Expecting { at };
    observations.push(Observation {
        kind: ObservationKind::Expecting,
        prediction: nodes[id].name.clone(),
        timestamp: at,
        log_line: None,
    });
}

fn expecting_pass(
    nodes: &mut Vec<Node>,
    _t0: DateTime<Utc>,
    name_to_id: &HashMap<String, usize>,
    observations: &mut Vec<Observation>,
) {
    // We iterate by index to avoid borrow issues
    for i in 0..nodes.len() {
        let &NodeState::Expecting { at: expected_at } = &nodes[i].state else { continue };

        // Both All and Any stage children identically:
        // stage every pending child whose `after` dependency is met.
        // Ordering comes ONLY from explicit `after` references.
        // Children without `after` use parent's expected time.
        let children = match &nodes[i].kind {
            NodeKind::Unit { .. } => continue,
            NodeKind::All { children, .. } | NodeKind::Any { children, .. } => children.clone(),
        };

        for &child_id in &children {
            if !matches!(nodes[child_id].state, NodeState::Pending) {
                continue;
            }
            let ref_time = resolve_ref_time(&nodes[child_id], nodes, name_to_id, expected_at);
            let Some(rt) = ref_time else { continue };
            expect_node(nodes, child_id, rt, observations);
        }
    }
}

/// Determine the reference time for a child node.
/// - Explicit `after`: waits for that binding to be Observed, uses its timestamp.
/// - No `after`: uses parent's expected time (immediate).
/// Returns None if the `after` dependency isn't observed yet.
fn resolve_ref_time(
    child: &Node,
    nodes: &[Node],
    name_to_id: &HashMap<String, usize>,
    parent_expected_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let after = match &child.kind {
        NodeKind::Unit { after, .. } => after.as_ref(),
        NodeKind::All { after, .. } => after.as_ref(),
        NodeKind::Any { after, .. } => after.as_ref(),
    };
    if let Some(ref_name) = after {
        let ref_id = name_to_id.get(ref_name)?;
        return match &nodes[*ref_id].state {
            NodeState::Pending | NodeState::Expecting { .. } | NodeState::Failed { .. } => None,
            // +1ns: "after X" means search starts just past X's observed timestamp,
            // so we never re-match X's own log entry.
            NodeState::Observed { at, .. } => Some(*at + Duration::nanoseconds(1)),
        };
    }
    // No explicit `after` — use parent's expected time
    Some(parent_expected_at)
}

async fn query_pass(
    nodes: &mut Vec<Node>,
    client: &LokiClient,
    base_query: &str,
    now: DateTime<Utc>,
    ingestion_slack: Duration,
    capture_store: &mut HashMap<String, String>,
    observations: &mut Vec<Observation>,
) {
    for i in 0..nodes.len() {
        let (pattern, timeout_ms, expected_at) = match &nodes[i] {
            Node {
                kind: NodeKind::Unit { pattern, after: _, timeout_ms },
                state: NodeState::Expecting { at },
                ..
            } => (pattern.clone(), *timeout_ms, *at),
            _ => continue,
        };

        let timeout = Duration::milliseconds(timeout_ms as i64);
        let window_end = expected_at + timeout;
        let deadline = window_end + ingestion_slack;

        // Query Loki FIRST — for retrospective runs the data already exists.
        let resolved_pattern = apply_captures(&pattern, capture_store);
        let query = format!("{} {}", base_query, resolved_pattern);
        match client.query_first(&query, expected_at, window_end).await {
            Err(e) => {
                error!(prediction = %nodes[i].name, error = %e, "loki query failed, will retry");
            }
            Ok(None) => {
                // No match found — check if we should give up.
                if now > deadline {
                    let name = nodes[i].name.clone();
                    nodes[i].state = NodeState::Failed { expected_at };
                    if is_critical_timeout(nodes, i) {
                        warn!(prediction = %name, "prediction timed out");
                    } else {
                        trace!(prediction = %name, "prediction timed out (non-critical)");
                    }
                    observations.push(Observation {
                        kind: ObservationKind::Failed,
                        prediction: name,
                        timestamp: now,
                        log_line: None,
                    });
                }
                // else: keep polling
            }
            Ok(Some(entry)) => {
                let name = nodes[i].name.clone();
                debug!(prediction = %name, ts = %entry.timestamp, "observed prediction");
                // Extract any named captures from the matched line and store them.
                let new_caps = extract_regexp_captures(&resolved_pattern, &entry.line);
                capture_store.extend(new_caps);
                nodes[i].state = NodeState::Observed {
                    at: entry.timestamp,
                    line: Some(entry.line.clone()),
                };
                observations.push(Observation {
                    kind: ObservationKind::Observed,
                    prediction: name,
                    timestamp: entry.timestamp,
                    log_line: Some(entry.line),
                });
            }
        }
    }
}

fn propagation_pass(nodes: &mut Vec<Node>, observations: &mut Vec<Observation>) {
    // Process from leaves upward: iterate by reverse id
    for i in (0..nodes.len()).rev() {
        if nodes[i].state.is_terminal() || matches!(nodes[i].state, NodeState::Pending) {
            continue;
        }

        match &nodes[i].kind {
            NodeKind::All { children, .. } => {
                let children = children.clone();
                let all_observed = children
                    .iter()
                    .all(|&c| matches!(nodes[c].state, NodeState::Observed { .. }));
                let any_failed = children
                    .iter()
                    .any(|&c| matches!(nodes[c].state, NodeState::Failed { .. }));

                if any_failed {
                    let &NodeState::Expecting { at: expected_at } = &nodes[i].state else { unreachable!() };
                    let at = Utc::now();
                    let name = nodes[i].name.clone();
                    nodes[i].state = NodeState::Failed { expected_at };
                    observations.push(Observation {
                        kind: ObservationKind::Failed,
                        prediction: name,
                        timestamp: at,
                        log_line: None,
                    });
                } else if all_observed {
                    // Timestamp = last child's observation
                    let at = children
                        .iter()
                        .filter_map(|&c| match &nodes[c].state {
                            NodeState::Observed { at, .. } => Some(*at),
                            _ => None,
                        })
                        .max()
                        .unwrap();
                    let name = nodes[i].name.clone();
                    debug!(group = %name, "All group observed");
                    nodes[i].state = NodeState::Observed { at, line: None };
                    observations.push(Observation {
                        kind: ObservationKind::Observed,
                        prediction: name,
                        timestamp: at,
                        log_line: None,
                    });
                }
            }
            NodeKind::Any { children, .. } => {
                let children = children.clone();
                let first_observed = children
                    .iter()
                    .find(|&&c| matches!(nodes[c].state, NodeState::Observed { .. }));
                let all_failed = children
                    .iter()
                    .all(|&c| matches!(nodes[c].state, NodeState::Failed { .. }));

                if all_failed {
                    let &NodeState::Expecting { at: expected_at } = &nodes[i].state else { unreachable!() };
                    let at = Utc::now();
                    let name = nodes[i].name.clone();
                    nodes[i].state = NodeState::Failed { expected_at };
                    observations.push(Observation {
                        kind: ObservationKind::Failed,
                        prediction: name,
                        timestamp: at,
                        log_line: None,
                    });
                } else if let Some(&c) = first_observed {
                    let NodeState::Observed { at, line } = &nodes[c].state else { unreachable!() };
                    let (at, line) = (*at, line.clone());
                    let name = nodes[i].name.clone();
                    debug!(group = %name, winner = %nodes[c].name, "Any group observed");
                    nodes[i].state = NodeState::Observed { at, line };
                    observations.push(Observation {
                        kind: ObservationKind::Observed,
                        prediction: name,
                        timestamp: at,
                        log_line: None,
                    });
                }
            }
            NodeKind::Unit { .. } => {}
        }
    }
}

fn is_critical_timeout(nodes: &[Node], node_id: usize) -> bool {
    let Some(parent_id) = nodes[node_id].parent else { return true };
    match &nodes[parent_id].kind {
        NodeKind::Unit { .. } | NodeKind::All { .. } => true,
        NodeKind::Any { children, .. } => children
            .iter()
            .filter(|&&c| c != node_id)
            .all(|&c| matches!(nodes[c].state, NodeState::Failed { .. })),
    }
}

fn find_failed_unit(
    nodes: &[Node],
    _name_to_id: &HashMap<String, usize>,
) -> (String, String, DateTime<Utc>, DateTime<Utc>) {
    for node in nodes {
        let NodeState::Failed { expected_at, .. } = &node.state else { continue };
        let NodeKind::Unit { pattern, timeout_ms, .. } = &node.kind else { continue };
        let search_start = *expected_at;
        let search_end = *expected_at + Duration::milliseconds(*timeout_ms as i64);
        return (node.name.clone(), pattern.clone(), search_start, search_end);
    }
    // Fallback if only group nodes failed (shouldn't happen with well-formed theories)
    let root = &nodes[0];
    let now = Utc::now();
    (root.name.clone(), String::new(), now, now)
}
