use std::collections::HashSet;

use regex::Regex;
use std::sync::OnceLock;
use thiserror::Error;

use crate::hypothesis::{GroupPrediction, PredictionDef, UnitPrediction};

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("duplicate binding: `{0}`")]
    DuplicateBinding(String),
    #[error("unknown reference `{reference}` in prediction `{prediction}`")]
    UnknownReference {
        prediction: String,
        reference: String,
    },
    #[error(
        "forward reference `{reference}` in prediction `{prediction}` (referenced prediction not yet observable)"
    )]
    ForwardReference {
        prediction: String,
        reference: String,
    },
    #[error("empty group prediction `{0}` (All/Any must have >= 1 child)")]
    EmptyGroup(String),
    #[error("root prediction must not have `after` set")]
    RootHasAfter,
    #[error("invalid regexp in prediction `{prediction}`: {error}")]
    InvalidRegexp { prediction: String, error: String },
    #[error(
        "capture `${{{capture}}}` used in prediction `{prediction}` but not guaranteed to be defined (not defined or defined in only some branches of an Any group)"
    )]
    UndefinedCapture { prediction: String, capture: String },
}

/// Validate a hypothesis before execution.
///
/// Checks:
/// 1. Binding uniqueness across entire tree
/// 2. Every `after` reference points to an existing binding
/// 3. No forward references (referenced binding must appear before referencing prediction in DFS order)
/// 4. All/Any groups must have at least one child
/// 5. Root prediction must not have `after`
/// 6. All `| regexp "..."` patterns have valid regex syntax
/// 7. `${name}` capture references are guaranteed to be defined before use
pub fn validate(hypothesis: &PredictionDef) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut all_bindings = HashSet::new();
    let mut binding_order = Vec::new();

    // Phase 1: collect all bindings, check uniqueness and non-empty groups
    collect_bindings(
        hypothesis,
        &mut all_bindings,
        &mut binding_order,
        &mut errors,
    );

    // Phase 2: check root has no `after`
    if hypothesis.after().is_some() {
        errors.push(ValidationError::RootHasAfter);
    }

    // Phase 3: check references exist and are not forward
    check_references(hypothesis, &binding_order, &mut errors);

    // Phase 4: validate all | regexp "..." patterns have valid regex syntax
    check_patterns(hypothesis, &mut errors);

    // Phase 5: validate capture scope — ${name} refs must be guaranteed-defined before use
    let mut guaranteed: HashSet<String> = HashSet::new();
    check_captures(hypothesis, &mut guaranteed, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn collect_bindings(
    pred: &PredictionDef,
    seen: &mut HashSet<String>,
    order: &mut Vec<String>,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(b) = pred.binding() {
        if !seen.insert(b.to_owned()) {
            errors.push(ValidationError::DuplicateBinding(b.to_owned()));
        } else {
            order.push(b.to_owned());
        }
    }

    match pred {
        PredictionDef::Unit(_) => {}
        PredictionDef::All(g) | PredictionDef::Any(g) => {
            let name = g.binding.as_deref().unwrap_or("<anonymous>");
            if g.predictions.is_empty() {
                errors.push(ValidationError::EmptyGroup(name.to_owned()));
            }
            for child in &g.predictions {
                collect_bindings(child, seen, order, errors);
            }
        }
    }
}

fn check_references(
    pred: &PredictionDef,
    binding_order: &[String],
    errors: &mut Vec<ValidationError>,
) {
    let pred_name = pred.binding().unwrap_or("<anonymous>");

    'after_check: {
        let Some(ref_name) = pred.after() else {
            break 'after_check;
        };
        let Some(ref_pos) = binding_order.iter().position(|b| b == ref_name) else {
            errors.push(ValidationError::UnknownReference {
                prediction: pred_name.to_owned(),
                reference: ref_name.to_owned(),
            });
            break 'after_check;
        };
        let Some(own_binding) = pred.binding() else {
            break 'after_check;
        };
        let Some(self_pos) = binding_order.iter().position(|b| b == own_binding) else {
            break 'after_check;
        };
        if ref_pos >= self_pos {
            errors.push(ValidationError::ForwardReference {
                prediction: pred_name.to_owned(),
                reference: ref_name.to_owned(),
            });
        }
    }

    match pred {
        PredictionDef::Unit(_) => {}
        PredictionDef::All(GroupPrediction { predictions, .. })
        | PredictionDef::Any(GroupPrediction { predictions, .. }) => {
            for child in predictions {
                check_references(child, binding_order, errors);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pattern helpers
// ---------------------------------------------------------------------------

/// Extract raw regex strings from `| regexp "..."` stages in a LogQL pattern.
fn regexp_strings(pattern: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"\|\s*regexp\s*"((?:[^"\\]|\\.)*)""#).unwrap());
    re.captures_iter(pattern)
        .map(|c| c[1].replace("\\\"", "\""))
        .collect()
}

/// Extract `(?P<name>...)` capture group names from a string.
fn capture_names(s: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\(\?P<([^>]+)>").unwrap());
    re.captures_iter(s).map(|c| c[1].to_owned()).collect()
}

/// Extract `${name}` template reference names from a string.
fn template_refs(s: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\$\{([^}]+)\}").unwrap());
    re.captures_iter(s).map(|c| c[1].to_owned()).collect()
}

// ---------------------------------------------------------------------------
// Phase 4: regexp syntax validation
// ---------------------------------------------------------------------------

fn check_patterns(pred: &PredictionDef, errors: &mut Vec<ValidationError>) {
    if let PredictionDef::Unit(u) = pred {
        check_unit_patterns(u, errors);
    }
    match pred {
        PredictionDef::Unit(_) => {}
        PredictionDef::All(g) | PredictionDef::Any(g) => {
            for child in &g.predictions {
                check_patterns(child, errors);
            }
        }
    }
}

fn check_unit_patterns(u: &UnitPrediction, errors: &mut Vec<ValidationError>) {
    let pred_name = u.binding.as_deref().unwrap_or("<anonymous>");
    for raw in regexp_strings(&u.pattern) {
        if let Err(e) = Regex::new(&raw) {
            errors.push(ValidationError::InvalidRegexp {
                prediction: pred_name.to_owned(),
                error: e.to_string(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 5: capture scope validation
// ---------------------------------------------------------------------------

/// Walk the theory DFS, tracking which captures are guaranteed to be defined.
/// - `All`: captures accumulate monotonically (all children run).
/// - `Any`: only captures defined by ALL branches (intersection) are guaranteed.
///
/// `${name}` used outside guaranteed scope → UndefinedCapture error.
fn check_captures(
    pred: &PredictionDef,
    guaranteed: &mut HashSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    match pred {
        PredictionDef::Unit(u) => {
            let pred_name = u.binding.as_deref().unwrap_or("<anonymous>");
            for cap in template_refs(&u.pattern) {
                if !guaranteed.contains(&cap) {
                    errors.push(ValidationError::UndefinedCapture {
                        prediction: pred_name.to_owned(),
                        capture: cap,
                    });
                }
            }
            for cap in capture_names(&u.pattern) {
                guaranteed.insert(cap);
            }
        }
        PredictionDef::All(g) => {
            for child in &g.predictions {
                check_captures(child, guaranteed, errors);
            }
        }
        PredictionDef::Any(g) => {
            let mut branch_new_caps: Vec<HashSet<String>> = Vec::new();
            for branch in &g.predictions {
                let mut branch_guaranteed = guaranteed.clone();
                check_captures(branch, &mut branch_guaranteed, errors);
                let new: HashSet<String> =
                    branch_guaranteed.difference(guaranteed).cloned().collect();
                branch_new_caps.push(new);
            }
            // Only intersection across ALL branches is guaranteed after the Any.
            if let Some((first, rest)) = branch_new_caps.split_first() {
                let intersection: HashSet<String> = first
                    .iter()
                    .filter(|c| rest.iter().all(|bc| bc.contains(*c)))
                    .cloned()
                    .collect();
                guaranteed.extend(intersection);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypothesis::*;

    fn unit(
        binding: Option<&str>,
        pattern: &str,
        after: Option<&str>,
        timeout_ms: u64,
    ) -> PredictionDef {
        PredictionDef::Unit(UnitPrediction {
            binding: binding.map(str::to_owned),
            pattern: pattern.to_owned(),
            after: after.map(str::to_owned),
            timeout_ms,
        })
    }

    fn all(binding: Option<&str>, preds: Vec<PredictionDef>) -> PredictionDef {
        PredictionDef::All(GroupPrediction {
            binding: binding.map(str::to_owned),
            after: None,
            predictions: preds,
        })
    }

    fn any(binding: Option<&str>, preds: Vec<PredictionDef>) -> PredictionDef {
        PredictionDef::Any(GroupPrediction {
            binding: binding.map(str::to_owned),
            after: None,
            predictions: preds,
        })
    }

    #[test]
    fn valid_hypothesis() {
        let theory = all(
            Some("root"),
            vec![
                unit(Some("a"), "|= \"hello\"", None, 5000),
                unit(Some("b"), "|= \"world\"", None, 5000),
                any(
                    Some("branch"),
                    vec![
                        unit(Some("c"), "|= \"left\"", None, 3000),
                        unit(Some("d"), "|= \"right\"", None, 3000),
                    ],
                ),
            ],
        );
        assert!(validate(&theory).is_ok());
    }

    #[test]
    fn duplicate_binding() {
        let theory = all(
            Some("root"),
            vec![
                unit(Some("a"), "|= \"x\"", None, 1000),
                unit(Some("a"), "|= \"y\"", None, 1000),
            ],
        );
        let errs = validate(&theory).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::DuplicateBinding(b) if b == "a"))
        );
    }

    #[test]
    fn unknown_reference() {
        let theory = all(
            Some("root"),
            vec![unit(Some("a"), "|= \"x\"", Some("nonexistent"), 1000)],
        );
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::UnknownReference { reference, .. } if reference == "nonexistent")));
    }

    #[test]
    fn forward_reference() {
        let theory = all(
            Some("root"),
            vec![
                unit(Some("a"), "|= \"x\"", Some("b"), 1000),
                unit(Some("b"), "|= \"y\"", None, 1000),
            ],
        );
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::ForwardReference { prediction, reference } if prediction == "a" && reference == "b")));
    }

    #[test]
    fn empty_group() {
        let theory = all(Some("root"), vec![any(Some("empty"), vec![])]);
        let errs = validate(&theory).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::EmptyGroup(name) if name == "empty"))
        );
    }

    #[test]
    fn root_group_has_after() {
        let theory = PredictionDef::All(GroupPrediction {
            binding: Some("root".to_owned()),
            after: Some("something".to_owned()),
            predictions: vec![unit(Some("a"), "|= \"x\"", None, 1000)],
        });
        let errs = validate(&theory).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::RootHasAfter))
        );
    }

    #[test]
    fn explicit_back_reference() {
        let theory = all(
            Some("root"),
            vec![
                unit(Some("a"), "|= \"x\"", None, 1000),
                unit(Some("b"), "|= \"y\"", None, 1000),
                unit(Some("c"), "|= \"z\"", Some("a"), 3000),
            ],
        );
        assert!(validate(&theory).is_ok());
    }

    // --- Phase 4: pattern validation ---

    #[test]
    fn invalid_regexp_pattern() {
        let theory = unit(
            Some("a"),
            "|= \"foo\" | regexp \"(?P<x>[unclosed)\"",
            None,
            1000,
        );
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(
            |e| matches!(e, ValidationError::InvalidRegexp { prediction, .. } if prediction == "a")
        ));
    }

    #[test]
    fn valid_regexp_pattern() {
        let theory = unit(
            Some("a"),
            "|= \"conn\" | regexp \"conn_id=(?P<conn_id>[a-f0-9]+)\"",
            None,
            1000,
        );
        assert!(validate(&theory).is_ok());
    }

    // --- Phase 5: capture scope validation ---

    #[test]
    fn capture_defined_before_use_ok() {
        let theory = all(
            Some("root"),
            vec![
                unit(
                    Some("a"),
                    "|= \"conn\" | regexp \"id=(?P<cid>\\\\w+)\"",
                    None,
                    1000,
                ),
                unit(Some("b"), "|= \"${cid}\"", Some("a"), 1000),
            ],
        );
        assert!(validate(&theory).is_ok());
    }

    #[test]
    fn capture_used_before_defined_fails() {
        let theory = all(
            Some("root"),
            vec![
                unit(Some("a"), "|= \"${cid}\"", None, 1000),
                unit(
                    Some("b"),
                    "|= \"conn\" | regexp \"id=(?P<cid>\\\\w+)\"",
                    Some("a"),
                    1000,
                ),
            ],
        );
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::UndefinedCapture { prediction, capture } if prediction == "a" && capture == "cid")));
    }

    #[test]
    fn partial_any_capture_used_after_fails() {
        // Branch A defines `cid`, branch B does not. Using ${cid} after Any should fail.
        let theory = all(
            Some("root"),
            vec![
                any(
                    Some("gate"),
                    vec![
                        unit(
                            Some("a"),
                            "|= \"conn\" | regexp \"id=(?P<cid>\\\\w+)\"",
                            None,
                            1000,
                        ),
                        unit(Some("b"), "|= \"other\"", None, 1000),
                    ],
                ),
                unit(Some("c"), "|= \"${cid}\"", Some("gate"), 1000),
            ],
        );
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::UndefinedCapture { prediction, capture } if prediction == "c" && capture == "cid")));
    }

    #[test]
    fn all_any_branches_define_capture_ok() {
        // Both branches of Any define `cid`, so it's guaranteed after.
        let theory = all(
            Some("root"),
            vec![
                any(
                    Some("gate"),
                    vec![
                        unit(
                            Some("a"),
                            "|= \"conn1\" | regexp \"id=(?P<cid>\\\\w+)\"",
                            None,
                            1000,
                        ),
                        unit(
                            Some("b"),
                            "|= \"conn2\" | regexp \"id=(?P<cid>\\\\w+)\"",
                            None,
                            1000,
                        ),
                    ],
                ),
                unit(Some("c"), "|= \"${cid}\"", Some("gate"), 1000),
            ],
        );
        assert!(validate(&theory).is_ok());
    }
}
