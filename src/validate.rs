use std::collections::HashSet;

use thiserror::Error;

use crate::theory::{GroupPrediction, PredictionDef};

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("duplicate binding: `{0}`")]
    DuplicateBinding(String),
    #[error("unknown reference `{reference}` in prediction `{prediction}`")]
    UnknownReference { prediction: String, reference: String },
    #[error("forward reference `{reference}` in prediction `{prediction}` (referenced prediction not yet observable)")]
    ForwardReference { prediction: String, reference: String },
    #[error("empty group prediction `{0}` (All/Any must have >= 1 child)")]
    EmptyGroup(String),
    #[error("root prediction must not have `after` set")]
    RootHasAfter,
}

/// Validate a theory before execution.
///
/// Checks:
/// 1. Binding uniqueness across entire tree
/// 2. Every `after` reference points to an existing binding
/// 3. No forward references (referenced binding must appear before referencing prediction in DFS order)
/// 4. All/Any groups must have at least one child
/// 5. Root prediction must not have `after`
pub fn validate(theory: &PredictionDef) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut all_bindings = HashSet::new();
    let mut binding_order = Vec::new();

    // Phase 1: collect all bindings, check uniqueness and non-empty groups
    collect_bindings(theory, &mut all_bindings, &mut binding_order, &mut errors);

    // Phase 2: check root has no `after`
    if let PredictionDef::Unit(u) = theory {
        if u.after.is_some() {
            errors.push(ValidationError::RootHasAfter);
        }
    }

    // Phase 3: check references exist and are not forward
    check_references(theory, &binding_order, &mut errors);

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
    // Check `after` on any node kind
    let after = pred.after();
    let pred_name = pred.binding().unwrap_or("<anonymous>");

    if let Some(ref_name) = after {
        if let Some(ref_pos) = binding_order.iter().position(|b| b == ref_name) {
            let self_pos = pred.binding().and_then(|b| binding_order.iter().position(|x| x == b));
            if let Some(sp) = self_pos {
                if ref_pos >= sp {
                    errors.push(ValidationError::ForwardReference {
                        prediction: pred_name.to_owned(),
                        reference: ref_name.to_owned(),
                    });
                }
            }
        } else {
            errors.push(ValidationError::UnknownReference {
                prediction: pred_name.to_owned(),
                reference: ref_name.to_owned(),
            });
        }
    }

    // Recurse into children
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theory::*;

    fn unit(binding: Option<&str>, pattern: &str, after: Option<&str>, timeout_ms: u64) -> PredictionDef {
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
    fn valid_theory() {
        let theory = all(Some("root"), vec![
            unit(Some("a"), "|= \"hello\"", None, 5000),
            unit(Some("b"), "|= \"world\"", None, 5000),
            any(Some("branch"), vec![
                unit(Some("c"), "|= \"left\"", None, 3000),
                unit(Some("d"), "|= \"right\"", None, 3000),
            ]),
        ]);
        assert!(validate(&theory).is_ok());
    }

    #[test]
    fn duplicate_binding() {
        let theory = all(Some("root"), vec![
            unit(Some("a"), "|= \"x\"", None, 1000),
            unit(Some("a"), "|= \"y\"", None, 1000),
        ]);
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::DuplicateBinding(b) if b == "a")));
    }

    #[test]
    fn unknown_reference() {
        let theory = all(Some("root"), vec![
            unit(Some("a"), "|= \"x\"", Some("nonexistent"), 1000),
        ]);
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::UnknownReference { reference, .. } if reference == "nonexistent")));
    }

    #[test]
    fn forward_reference() {
        let theory = all(Some("root"), vec![
            unit(Some("a"), "|= \"x\"", Some("b"), 1000),
            unit(Some("b"), "|= \"y\"", None, 1000),
        ]);
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::ForwardReference { prediction, reference } if prediction == "a" && reference == "b")));
    }

    #[test]
    fn empty_group() {
        let theory = all(Some("root"), vec![
            any(Some("empty"), vec![]),
        ]);
        let errs = validate(&theory).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::EmptyGroup(name) if name == "empty")));
    }

    #[test]
    fn explicit_back_reference() {
        let theory = all(Some("root"), vec![
            unit(Some("a"), "|= \"x\"", None, 1000),
            unit(Some("b"), "|= \"y\"", None, 1000),
            unit(Some("c"), "|= \"z\"", Some("a"), 3000),
        ]);
        assert!(validate(&theory).is_ok());
    }
}
