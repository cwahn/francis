# francis

[![Crates.io](https://img.shields.io/crates/v/francis.svg)](https://crates.io/crates/francis)
[![CI](https://github.com/cwahn/francis/actions/workflows/ci.yml/badge.svg)](https://github.com/cwahn/francis/actions/workflows/ci.yml)

**Log-based hypothesis verifier.** Declare a tree of ordered predictions about expected log events, run them against a [Loki](https://grafana.com/oss/loki/) instance, and get a clear pass/fail report with a full audit trail.

## Motivation

Integration tests often need to verify that a sequence of log events occurred in the right order within a time budget — e.g. "after the server starts, a connection is accepted, then a bi-stream is received within 60 seconds". Francis lets you express this as structured JSON and run it as a CI step or post-deploy check.

## Installation

```sh
cargo install francis
```

## Usage

```
francis [OPTIONS] <HYPOTHESIS>
```

| Option | Default | Description |
|---|---|---|
| `--t0 <RFC3339\|now>` | `now` | Reference start time for the root prediction |
| `--loki-url <URL>` | from hypothesis | Override the Loki URL |
| `--base-query <LOGQL>` | from hypothesis | Override the base LogQL selector |
| `--output <text\|json>` | `text` | Output format |
| `--dry-run` | | Validate the hypothesis without executing |

Exit code `0` = hypothesis verified, `1` = falsified or error.

Log verbosity is controlled via `RUST_LOG` (e.g. `RUST_LOG=debug`).

## Hypothesis format

A hypothesis is a JSON file describing a tree of predictions. The top-level `hypothesis` field is a `PredictionDef` — either a `Unit`, an `All`, or an `Any`.

### JSON representation (externally tagged)

```json
{ "Unit": { ... } }
{ "All":  { ... } }
{ "Any":  { ... } }
```

### `Unit` — a single expected log line

```json
{
  "Unit": {
    "binding":    "server_started",
    "pattern":    "|= \"provision started\"",
    "after":      "prev_binding",
    "timeout_ms": 90000
  }
}
```

| Field | Required | Description |
|---|---|---|
| `binding` | no | Name for this prediction (used by `after` references) |
| `pattern` | yes | LogQL pipeline appended to `base_query`, e.g. `\|= "foo"` |
| `after` | no | Wait for this binding to be observed before starting the timeout |
| `timeout_ms` | yes | How long to wait for the log line |

### `All` — every child must be observed

```json
{
  "All": {
    "binding": "root",
    "after": null,
    "predictions": [ ... ]
  }
}
```

### `Any` — at least one child must be observed

```json
{
  "Any": {
    "binding": "outcome",
    "after": "some_binding",
    "predictions": [ ... ]
  }
}
```

### Named captures

A `Unit` pattern can include a `| regexp "(?P<name>...)"` LogQL stage. When the prediction is observed:
1. The regex is run client-side against the matched log line.
2. Each named capture group is stored under its name.
3. Downstream `pattern` fields can reference it with `${name}`.

Example — correlate all events to the same connection:

```json
{
  "Unit": {
    "binding": "conn_accepted",
    "pattern": "|= \"connection accepted\" | regexp \"conn_id=(?P<conn_id>[a-f0-9-]+)\"",
    "timeout_ms": 120000
  }
},
{
  "Unit": {
    "binding": "first_request",
    "pattern": "|= \"request received\" |= \"${conn_id}\"",
    "after": "conn_accepted",
    "timeout_ms": 30000
  }
}
```

**Validation rules for captures:**
- A capture must be defined (observed) before it is referenced.
- Inside an `Any` group, a capture is only considered guaranteed if **all** branches of that `Any` define it. Captures defined in only some branches cannot be referenced after the group.

## Full example

See [theories/theta_bistream_h3.json](theories/theta_bistream_h3.json) for a complete real-world hypothesis verifying H3 bi-stream handshake behaviour.

```json
{
  "source": {
    "url": "http://localhost:3100",
    "base_query": "{service_name=\"my-service\"}"
  },
  "poll_interval_ms": 500,
  "ingestion_slack_ms": 10000,
  "hypothesis": {
    "All": {
      "binding": "root",
      "predictions": [
        {
          "Unit": {
            "binding": "started",
            "pattern": "|= \"server started\"",
            "timeout_ms": 60000
          }
        },
        {
          "Any": {
            "binding": "outcome",
            "after": "started",
            "predictions": [
              {
                "Unit": {
                  "binding": "ready",
                  "pattern": "|= \"ready\"",
                  "timeout_ms": 30000
                }
              },
              {
                "Unit": {
                  "binding": "error",
                  "pattern": "|= \"startup error\"",
                  "timeout_ms": 30000
                }
              }
            ]
          }
        }
      ]
    }
  }
}
```

## Output

**Text (default):**
```
✓ PASS — all predictions observed
  [Expecting] root                     at 12:00:00.000Z
  [Expecting] started                  at 12:00:00.000Z
  [Observed ] started                  at 12:00:01.234Z  2024-01-01T12:00:01Z server started
  ...
```

**JSON (`--output json`):**
```json
{
  "pass": {
    "observations": [
      { "kind": "expecting", "prediction": "root",    "timestamp": "...", "log_line": null },
      { "kind": "observed", "prediction": "started", "timestamp": "...", "log_line": "..." }
    ]
  }
}
```

## Validation

Francis validates the hypothesis at startup and rejects:
- Duplicate binding names
- Unknown or forward `after` references
- Empty `All`/`Any` groups
- `after` set on the root prediction
- Invalid regex syntax in `| regexp "..."` stages
- `${name}` capture references that aren't guaranteed to be defined

## License

MIT
