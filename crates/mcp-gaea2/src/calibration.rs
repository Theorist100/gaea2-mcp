//! Measuring what a node actually produces, as opposed to what its parameters say.
//!
//! A declared range tells a caller what is accepted, and the shipped scenes tell them what other
//! people chose, but neither says how tall the result comes out. `Mountain` at `Scale 0.16` with
//! `Height 0.42` occupies 3% of the vertical range: the height parameter scales with the size of
//! the mountain, so a small one is a low one no matter what is asked for. Nothing in the metadata
//! says so, and the map it was on came out flat with no error anywhere.
//!
//! The measurement is cheap because Gaea writes one file per node marked for saving, so a single
//! build can carry every generator at once - forty nodes, forty heightfields, one launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What one node produced at one setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    /// Node type measured.
    pub node_type: String,
    /// Property varied, absent when the node was built at its defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    /// Value the property was set to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Seeds the node was built with.
    pub seeds: Vec<i64>,
    /// Share of the full vertical range the output occupies, median across seeds, 0..1.
    pub amplitude: f64,
    /// Lowest and highest amplitude across seeds: how much the seed alone moves the result.
    pub amplitude_low: f64,
    pub amplitude_high: f64,
    /// Mean height of the output, median across seeds, 0..1. Distinguishes a field that fills
    /// the range from one that sits low with a single peak.
    pub mean_level: f64,
}

/// Everything measured in one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calibration {
    /// Gaea build the measurements came from; they do not carry across versions.
    pub gaea_version: String,
    /// When the run happened.
    pub measured_at: String,
    /// Build resolution used.
    pub resolution: u32,
    /// Every measurement, in the order taken.
    pub measurements: Vec<Measurement>,
}

impl Calibration {
    /// Measurements for one node type.
    pub fn for_node<'a>(&'a self, node_type: &str) -> Vec<&'a Measurement> {
        self.measurements
            .iter()
            .filter(|m| m.node_type == node_type)
            .collect()
    }

    /// What the node produced at its defaults, if that was measured.
    pub fn defaults_for<'a>(&'a self, node_type: &str) -> Option<&'a Measurement> {
        self.measurements
            .iter()
            .find(|m| m.node_type == node_type && m.property.is_none())
    }
}

/// Where a calibration is kept: beside the generated schema, one file per machine.
pub fn default_path(output_dir: &Path) -> PathBuf {
    output_dir.join("gaea_calibration.json")
}

/// Read a calibration from disk, returning None when there is none rather than failing: a server
/// without one is a server that has not measured yet, not a broken one.
pub fn load(path: &Path) -> Option<Calibration> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Fold new measurements into whatever is already on disk.
///
/// A run measures one property at a time, so a second run would otherwise throw away the first.
/// A measurement of the same node at the same setting is replaced rather than duplicated; one
/// from an older Gaea build is dropped entirely, since amplitudes do not carry across versions.
pub fn merge(existing: Option<Calibration>, fresh: Calibration) -> Calibration {
    let Some(previous) = existing.filter(|c| c.gaea_version == fresh.gaea_version) else {
        return fresh;
    };

    let identity = |m: &Measurement| {
        (
            m.node_type.clone(),
            m.property.clone(),
            m.value.map(|v| (v * 10_000.0).round() as i64),
        )
    };

    let mut merged = previous;
    for measurement in fresh.measurements {
        let key = identity(&measurement);
        match merged.measurements.iter().position(|m| identity(m) == key) {
            Some(at) => merged.measurements[at] = measurement,
            None => merged.measurements.push(measurement),
        }
    }
    merged.measured_at = fresh.measured_at;
    merged.resolution = fresh.resolution;
    merged
}

/// Write a calibration.
pub fn save(path: &Path, calibration: &Calibration) -> Result<(), String> {
    let text = serde_json::to_string_pretty(calibration)
        .map_err(|e| format!("Cannot serialise the calibration: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

/// Amplitude and mean level of one built heightfield, both 0..1 of the full vertical range.
///
/// The file is read at its own scale - 16-bit raw covers 0..65535 - so that two nodes measured in
/// the same run are comparable. Normalising each field to its own range, which is what a shape
/// report wants, would make every node look identical here.
pub fn measure(path: &Path) -> Result<(f64, f64), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if bytes.len() < 2 {
        return Err(format!("{} holds no samples", path.display()));
    }

    let full = f64::from(u16::MAX);
    let mut low = f64::MAX;
    let mut high = f64::MIN;
    let mut sum = 0.0;
    let mut count = 0.0;
    for pair in bytes.chunks_exact(2) {
        let value = f64::from(u16::from_le_bytes([pair[0], pair[1]]));
        low = low.min(value);
        high = high.max(value);
        sum += value;
        count += 1.0;
    }

    Ok(((high - low) / full, sum / count / full))
}

/// Median of a set of measurements, which is steadier than a mean when one seed lands oddly.
pub fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// Nodes worth measuring: those that generate a heightfield of their own.
///
/// A node that needs an input produces nothing on its own, and one that outputs colour or a mask
/// has no vertical range to speak of. Both would fill the table with zeroes.
pub fn measurable_nodes() -> Vec<&'static str> {
    let mut nodes: Vec<&'static str> = Vec::new();
    for (category, members) in crate::gaea_schema_generated::NODE_CATEGORIES {
        if *category != "Primitive" && *category != "Terrain" {
            continue;
        }
        for node in *members {
            // A generator owns a Seed; without one the node is a transform wearing a category.
            if crate::schema::is_generator_node(node) {
                nodes.push(node);
            }
        }
    }
    nodes.sort_unstable();
    nodes
}

/// The values to sweep a property through, taken from its declared range.
///
/// Endpoints included: the interesting behaviour of a scaling parameter shows at the extremes,
/// and the midpoint alone is what made a hill invisible in the first place.
pub fn sweep_values(node_type: &str, property: &str, steps: usize) -> Vec<f64> {
    let Some(declared) = crate::schema::find_property(node_type, property) else {
        return Vec::new();
    };
    let (Some(min), Some(max)) = (declared.min, declared.max) else {
        return Vec::new();
    };
    if steps < 2 || max <= min {
        return vec![min];
    }

    (0..steps)
        .map(|i| {
            let t = i as f64 / (steps - 1) as f64;
            let value = min + (max - min) * t;
            (value * 10_000.0).round() / 10_000.0
        })
        .collect()
}

/// Group measurements by node type for reporting.
pub fn by_node(measurements: &[Measurement]) -> BTreeMap<&str, Vec<&Measurement>> {
    let mut grouped: BTreeMap<&str, Vec<&Measurement>> = BTreeMap::new();
    for m in measurements {
        grouped.entry(m.node_type.as_str()).or_default().push(m);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_takes_the_middle() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut []), 0.0);
    }

    #[test]
    fn a_sweep_covers_both_ends() {
        let values = sweep_values("Mountain", "Height", 3);
        assert_eq!(values.len(), 3);
        assert!((values[0] - 0.0001).abs() < 1e-6, "starts at the minimum");
        assert!((values[2] - 3.0).abs() < 1e-6, "ends at the maximum");
    }

    #[test]
    fn a_sweep_of_an_unknown_property_is_empty() {
        assert!(sweep_values("Mountain", "NoSuchThing", 3).is_empty());
    }

    #[test]
    fn only_generators_are_measurable() {
        let nodes = measurable_nodes();
        assert!(nodes.contains(&"Mountain"), "Mountain generates terrain");
        assert!(
            !nodes.contains(&"Erosion2"),
            "Erosion2 transforms an input and generates nothing alone"
        );
    }
}
