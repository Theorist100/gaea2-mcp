//! Reading a built heightfield back and describing the terrain it holds.
//!
//! A build result is a pile of files, and the questions that matter are about the land: how much
//! of it is flat enough to build on, whether the edges stand above the middle, how tall it is in
//! metres. Answering those by hand meant a throwaway script every time.

use std::path::Path;

use serde::Serialize;

/// A heightfield loaded from disk, normalised to 0..1.
pub struct Heightfield {
    /// Samples per side.
    pub size: usize,
    /// Row-major samples, 0..1.
    pub samples: Vec<f64>,
    /// Raw range before normalisation, for reporting.
    pub raw_min: f64,
    pub raw_max: f64,
}

/// How the terrain reads against what a map needs.
#[derive(Serialize)]
pub struct TerrainReport {
    /// Samples per side.
    pub resolution: usize,
    /// Metres from one edge of the map to the other.
    pub metres_across: f64,
    /// Metres between the lowest and highest point after scaling.
    pub relief_metres: f64,
    /// Raw sample range, useful for spotting a flat or clipped field.
    pub raw_min: f64,
    pub raw_max: f64,
    /// True when every sample is the same: the graph computed nothing.
    pub is_flat: bool,
    /// Share of the playable middle no steeper than `gentle_degrees`.
    pub centre_gentle_percent: f64,
    /// Share of the whole map no steeper than `gentle_degrees`.
    pub overall_gentle_percent: f64,
    /// How much higher the outer frame sits than the middle, in metres.
    /// Positive means a basin - raised edges around a flat middle.
    pub edge_lift_metres: f64,
    /// Mean slope of the playable middle, in degrees.
    pub centre_mean_slope_degrees: f64,
    /// Share of the map in each height class, low to high.
    pub height_classes: HeightClasses,
    /// Mean height per row and per column, north to south and west to east, in metres.
    /// A dip in the middle of one of these is a valley across that axis.
    pub profile_north_south: Vec<f64>,
    pub profile_west_east: Vec<f64>,
}

/// The three classes the game reasons about.
#[derive(Serialize)]
pub struct HeightClasses {
    /// Bottom third of the range.
    pub lowland_percent: f64,
    /// Middle third.
    pub plain_percent: f64,
    /// Top third: what the radar and air defence bonuses key off.
    pub commanding_percent: f64,
}

/// Load a heightfield from a 16-bit raw dump or a PNG.
pub fn load(path: &Path) -> Result<Heightfield, String> {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let values: Vec<f64> = match extension.as_str() {
        "raw" | "r16" => {
            let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {path:?}: {e}"))?;
            bytes
                .chunks_exact(2)
                .map(|pair| f64::from(u16::from_le_bytes([pair[0], pair[1]])))
                .collect()
        },
        "png" => read_png(path)?,
        other => {
            return Err(format!(
                "Cannot read '{other}' files; expected .raw or .png"
            ))
        },
    };

    if values.is_empty() {
        return Err("The file holds no samples".to_string());
    }

    let size = (values.len() as f64).sqrt().round() as usize;
    if size * size != values.len() {
        return Err(format!(
            "{} samples do not form a square heightfield",
            values.len()
        ));
    }

    let raw_min = values.iter().copied().fold(f64::MAX, f64::min);
    let raw_max = values.iter().copied().fold(f64::MIN, f64::max);
    let span = raw_max - raw_min;
    let samples = if span > 0.0 {
        values.iter().map(|v| (v - raw_min) / span).collect()
    } else {
        vec![0.0; values.len()]
    };

    Ok(Heightfield {
        size,
        samples,
        raw_min,
        raw_max,
    })
}

fn read_png(path: &Path) -> Result<Vec<f64>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot read {path:?}: {e}"))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // Gaea writes its PNG8 outputs with a palette, so the samples only become readable once the
    // palette is expanded. Sub-byte greyscale is expanded for the same reason.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("Not a readable PNG: {e}"))?;

    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|e| format!("Cannot decode PNG: {e}"))?;

    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        // EXPAND turns a palette into RGB before the frame is handed over, so this is unreachable
        // unless the decoder changes behaviour.
        png::ColorType::Indexed => {
            return Err("The palette in this PNG was not expanded".to_string());
        },
    };

    // Only the first channel carries height; the rest is colour or alpha.
    let data = &buffer[..info.buffer_size()];
    let values = match info.bit_depth {
        png::BitDepth::Sixteen => data
            .chunks_exact(2 * channels)
            .map(|px| f64::from(u16::from_be_bytes([px[0], px[1]])))
            .collect(),
        png::BitDepth::Eight => data
            .chunks_exact(channels)
            .map(|px| f64::from(px[0]))
            .collect(),
        depth => return Err(format!("Unsupported bit depth {depth:?}")),
    };

    Ok(values)
}

impl Heightfield {
    /// Describe the terrain against the shape a playable map needs.
    ///
    /// `centre_fraction` is the share of the map treated as the playable middle: 0.6 keeps the
    /// inner 60% and calls the surrounding band the frame.
    pub fn report(
        &self,
        metres_across: f64,
        metres_up: f64,
        gentle_degrees: f64,
        centre_fraction: f64,
    ) -> TerrainReport {
        let n = self.size;
        let step = metres_across / n as f64;
        let height: Vec<f64> = self.samples.iter().map(|v| v * metres_up).collect();
        let at = |row: usize, col: usize| height[row * n + col];

        // Slope from the difference to the next sample in each direction.
        let mut slope = vec![0.0; n * n];
        for row in 0..n {
            for col in 0..n {
                let dz_dx = if col + 1 < n {
                    at(row, col + 1) - at(row, col)
                } else {
                    0.0
                };
                let dz_dy = if row + 1 < n {
                    at(row + 1, col) - at(row, col)
                } else {
                    0.0
                };
                slope[row * n + col] = (dz_dx.hypot(dz_dy) / step).atan().to_degrees();
            }
        }

        let margin = ((1.0 - centre_fraction.clamp(0.1, 1.0)) / 2.0 * n as f64) as usize;
        let (lo, hi) = (margin, n - margin);

        let mut centre_gentle = 0usize;
        let mut centre_total = 0usize;
        let mut centre_slope_sum = 0.0;
        let mut centre_height_sum = 0.0;
        let mut frame_height_sum = 0.0;
        let mut frame_total = 0usize;
        let mut overall_gentle = 0usize;

        for row in 0..n {
            for col in 0..n {
                let index = row * n + col;
                if slope[index] <= gentle_degrees {
                    overall_gentle += 1;
                }
                let inside = row >= lo && row < hi && col >= lo && col < hi;
                if inside {
                    centre_total += 1;
                    centre_slope_sum += slope[index];
                    centre_height_sum += height[index];
                    if slope[index] <= gentle_degrees {
                        centre_gentle += 1;
                    }
                } else {
                    frame_total += 1;
                    frame_height_sum += height[index];
                }
            }
        }

        let relief = height.iter().copied().fold(f64::MIN, f64::max)
            - height.iter().copied().fold(f64::MAX, f64::min);
        let third = relief / 3.0;
        let floor = height.iter().copied().fold(f64::MAX, f64::min);
        let mut classes = [0usize; 3];
        for value in &height {
            let band = if relief <= 0.0 {
                1
            } else if *value < floor + third {
                0
            } else if *value < floor + 2.0 * third {
                1
            } else {
                2
            };
            classes[band] += 1;
        }
        let total = (n * n) as f64;

        let rows: Vec<f64> = (0..n)
            .map(|row| height[row * n..(row + 1) * n].iter().sum::<f64>() / n as f64)
            .collect();
        let cols: Vec<f64> = (0..n)
            .map(|col| (0..n).map(|row| at(row, col)).sum::<f64>() / n as f64)
            .collect();

        TerrainReport {
            resolution: n,
            metres_across,
            relief_metres: relief,
            raw_min: self.raw_min,
            raw_max: self.raw_max,
            is_flat: self.raw_max <= self.raw_min,
            centre_gentle_percent: percentage(centre_gentle, centre_total),
            overall_gentle_percent: percentage(overall_gentle, n * n),
            edge_lift_metres: if frame_total == 0 || centre_total == 0 {
                0.0
            } else {
                frame_height_sum / frame_total as f64 - centre_height_sum / centre_total as f64
            },
            centre_mean_slope_degrees: if centre_total == 0 {
                0.0
            } else {
                centre_slope_sum / centre_total as f64
            },
            height_classes: HeightClasses {
                lowland_percent: classes[0] as f64 / total * 100.0,
                plain_percent: classes[1] as f64 / total * 100.0,
                commanding_percent: classes[2] as f64 / total * 100.0,
            },
            profile_north_south: downsample(&rows, 16),
            profile_west_east: downsample(&cols, 16),
        }
    }
}

fn percentage(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// Shrink a profile to a readable number of points, averaging within each bucket.
fn downsample(values: &[f64], buckets: usize) -> Vec<f64> {
    if values.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let per = values.len().div_ceil(buckets);
    values
        .chunks(per)
        .map(|chunk| (chunk.iter().sum::<f64>() / chunk.len() as f64 * 10.0).round() / 10.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(size: usize, f: impl Fn(usize, usize) -> f64) -> Heightfield {
        let samples: Vec<f64> = (0..size * size).map(|i| f(i / size, i % size)).collect();
        Heightfield {
            size,
            samples,
            raw_min: 0.0,
            raw_max: 1.0,
        }
    }

    #[test]
    fn a_flat_field_is_all_gentle() {
        let report = field(32, |_, _| 0.5).report(4800.0, 300.0, 5.0, 0.6);
        assert_eq!(report.centre_gentle_percent, 100.0);
        assert_eq!(report.relief_metres, 0.0);
        assert_eq!(report.edge_lift_metres, 0.0);
    }

    #[test]
    fn a_basin_reports_raised_edges() {
        // high at the border, low in the middle
        let report = field(64, |row, col| {
            let dr = (row as f64 - 31.5).abs() / 31.5;
            let dc = (col as f64 - 31.5).abs() / 31.5;
            dr.max(dc)
        })
        .report(4800.0, 300.0, 5.0, 0.6);

        assert!(
            report.edge_lift_metres > 0.0,
            "edges should sit above the middle: {}",
            report.edge_lift_metres
        );
        assert!(report.centre_gentle_percent > 0.0);
    }

    #[test]
    fn a_hill_in_the_middle_reports_sunken_edges() {
        let report = field(64, |row, col| {
            let dr = (row as f64 - 31.5) / 31.5;
            let dc = (col as f64 - 31.5) / 31.5;
            (1.0 - dr.hypot(dc)).max(0.0)
        })
        .report(4800.0, 300.0, 5.0, 0.6);

        assert!(
            report.edge_lift_metres < 0.0,
            "a central hill means the frame is lower: {}",
            report.edge_lift_metres
        );
    }

    #[test]
    fn height_classes_split_the_range() {
        let report = field(30, |row, _| row as f64 / 29.0).report(4800.0, 300.0, 5.0, 0.6);
        let classes = &report.height_classes;
        let total = classes.lowland_percent + classes.plain_percent + classes.commanding_percent;
        assert!((total - 100.0).abs() < 0.001, "classes must cover the map");
        assert!(classes.commanding_percent > 0.0);
    }
}
