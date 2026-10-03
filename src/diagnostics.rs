//! Per-layer interpretation diagnostics.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use crate::flatten::{FlattenError, FlattenIssue, SpecFeature};
use crate::layer::LayerData;

/// What the library could and could not interpret in one layer.
///
/// A layer is *complete* when it has no parse errors, flattens without an
/// error and reports no flatten issues. Saving an edited, incomplete layer
/// may drop the content that was not understood.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct LayerDiagnostics {
    /// Commands the parser rejected (kept out of the layer).
    pub parse_errors: Vec<String>,
    /// Why the layer could not be flattened at all.
    pub flatten_error: Option<FlattenError>,
    /// Per-object problems found while flattening.
    pub flatten_issues: Vec<FlattenIssue>,
    /// Spec features the layer uses.
    pub features: BTreeMap<SpecFeature, usize>,
}

impl LayerDiagnostics {
    /// Diagnoses `data` (this flattens the layer).
    pub fn of(data: &LayerData) -> Self {
        let (parse_errors, flat) = match data {
            LayerData::Gerber(g) => (g.parse_errors.clone(), g.flatten()),
            LayerData::Excellon(e) => {
                let errors = e
                    .header
                    .iter()
                    .chain(&e.commands)
                    .filter_map(|c| c.as_ref().err().map(ToString::to_string))
                    .collect();
                (errors, e.flatten())
            }
            LayerData::Info(_) => return Self::default(),
        };
        let mut diagnostics = LayerDiagnostics {
            parse_errors,
            ..Self::default()
        };
        match flat {
            Ok(flat) => {
                // Excellon parse errors are already listed above.
                diagnostics.flatten_issues = flat
                    .issues
                    .into_iter()
                    .filter(|i| !matches!(i.kind, crate::flatten::FlattenIssueKind::Unparsed(_)))
                    .collect();
                diagnostics.features = flat.features;
            }
            Err(e) => diagnostics.flatten_error = Some(e),
        }
        diagnostics
    }

    /// No parse errors, flatten errors or flatten issues.
    pub fn is_complete(&self) -> bool {
        self.parse_errors.is_empty()
            && self.flatten_error.is_none()
            && self.flatten_issues.is_empty()
    }
}

impl Display for LayerDiagnostics {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.is_complete() {
            writeln!(f, "  complete")?;
        }
        for e in &self.parse_errors {
            writeln!(f, "  parse error: {e}")?;
        }
        if let Some(e) = &self.flatten_error {
            writeln!(f, "  not flattened: {e}")?;
        }
        for i in &self.flatten_issues {
            writeln!(f, "  issue: {i}")?;
        }
        if !self.features.is_empty() {
            let list: Vec<String> = self
                .features
                .iter()
                .map(|(feature, n)| format!("{feature:?}×{n}"))
                .collect();
            writeln!(f, "  features: {}", list.join(", "))?;
        }
        Ok(())
    }
}
