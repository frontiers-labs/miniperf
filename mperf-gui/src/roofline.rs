use anyhow::{Context, Result, bail};
use mperf_data::{MemoryLevelCalibration, RooflineCalibration, RooflineMethodInfo};

use crate::sql::{Connection, Value, as_f64, as_i64, as_text, row_values, table_columns};

const LABEL_ASSET_PREFIX: &str = "roofline-label:";

pub(crate) fn roofline_label_asset(text: &str) -> String {
    format!("{LABEL_ASSET_PREFIX}{text}")
}

pub(crate) fn roofline_label_svg(path: &str) -> Option<Vec<u8>> {
    let text = path.strip_prefix(LABEL_ASSET_PREFIX)?;
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    Some(
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 176 18"><text x="88" y="13" text-anchor="middle" font-family="sans-serif" font-size="11" font-weight="500" fill="black">{escaped}</text></svg>"#,
        )
        .into_bytes(),
    )
}

#[derive(Debug)]
pub struct RooflineData {
    pub loops: Vec<RooflineLoop>,
    pub calibration: Option<RooflineCalibration>,
    pub method: Option<RooflineMethodInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RooflineLoop {
    pub function_name: String,
    pub file_name: String,
    pub line: usize,
    pub scalar_int_ops: Option<f64>,
    pub scalar_int_ai: Option<f64>,
    pub scalar_float_ops: Option<f64>,
    pub scalar_float_ai: Option<f64>,
    pub scalar_double_ops: Option<f64>,
    pub scalar_double_ai: Option<f64>,
    pub vector_int_ops: Option<f64>,
    pub vector_int_ai: Option<f64>,
    pub vector_float_ops: Option<f64>,
    pub vector_float_ai: Option<f64>,
    pub vector_double_ops: Option<f64>,
    pub vector_double_ai: Option<f64>,
    pub timing_samples: Option<u64>,
    pub timing_relative_error: Option<f64>,
    pub timing_quality: Option<String>,
    pub module_offset: Option<String>,
    pub trip_count: Option<u64>,
    pub thread_count: Option<u32>,
    pub duration_ns: Option<u64>,
    pub cpu_time_ns: Option<u64>,
}

impl RooflineData {
    pub fn load(
        connection: &Connection,
        calibration: Option<RooflineCalibration>,
        method: Option<RooflineMethodInfo>,
    ) -> RooflineData {
        Self {
            loops: load_loops(connection).unwrap_or_default(),
            calibration,
            method,
        }
    }

    /// Calibrated all-core bandwidth roofs that match this recording's
    /// intensity axis. Architectural traffic uses the complete cache-aware
    /// hierarchy, while DRAM traffic uses only the DRAM-sized streaming roof.
    pub fn bandwidth_roofs(&self) -> Vec<(&str, f64)> {
        let Some(calibration) = self.calibration.as_ref() else {
            return Vec::new();
        };
        self.select_roofs(
            &calibration.memory_levels,
            calibration.memory_gbytes_per_second,
        )
    }

    /// The single-thread ceilings, when the calibration measured them: FP64
    /// peak and the bandwidth roofs on this recording's intensity axis.
    pub fn single_thread_roofs(&self) -> Option<(f64, Vec<(&str, f64)>)> {
        let single = self.calibration.as_ref()?.single_thread.as_ref()?;
        let compute = finite_positive(single.fp64_gflops)?;
        Some((
            compute,
            self.select_roofs(&single.memory_levels, single.memory_gbytes_per_second),
        ))
    }

    fn select_roofs<'a>(
        &self,
        levels: &'a [MemoryLevelCalibration],
        dram: f64,
    ) -> Vec<(&'a str, f64)> {
        let Some(method) = self.method.as_ref() else {
            return Vec::new();
        };
        let mut roofs = match method.traffic.as_str() {
            "architectural" => levels
                .iter()
                .map(|level| (level.level.as_str(), level.gbytes_per_second))
                .collect::<Vec<_>>(),
            "dram" | "dram-model" => vec![("DRAM", dram)],
            _ => Vec::new(),
        };
        roofs.retain(|(_, bandwidth)| bandwidth.is_finite() && *bandwidth > 0.0);
        roofs.sort_by(|left, right| right.1.total_cmp(&left.1));
        roofs
    }

    /// Whether a loop is rated against the single-thread ceilings: it ran on
    /// one thread and the calibration measured them.
    pub fn uses_single_thread_roofs(&self, loop_data: &RooflineLoop) -> bool {
        loop_data.thread_count == Some(1) && self.single_thread_roofs().is_some()
    }

    pub fn efficiency(&self, loop_data: &RooflineLoop) -> Option<f64> {
        let (compute, roofs) = if self.uses_single_thread_roofs(loop_data) {
            self.single_thread_roofs()?
        } else {
            (
                self.calibration.as_ref()?.fp64_gflops,
                self.bandwidth_roofs(),
            )
        };
        let bandwidth = roofs
            .into_iter()
            .map(|(_, bandwidth)| bandwidth)
            .max_by(f64::total_cmp)?;
        let observed = loop_data.fp64_gflops()?;
        let intensity = loop_data.fp64_arithmetic_intensity()?;
        let roof = finite_positive(compute.min(bandwidth * intensity))?;
        (observed / roof).is_finite().then_some(observed / roof)
    }
}

impl RooflineLoop {
    pub fn fp64_gflops(&self) -> Option<f64> {
        finite_positive(
            sum_optional(self.scalar_double_ops, self.vector_double_ops)? / 1_000_000_000.0,
        )
    }

    pub fn fp64_arithmetic_intensity(&self) -> Option<f64> {
        finite_positive(sum_optional(self.scalar_double_ai, self.vector_double_ai)?)
    }
}

fn load_loops(connection: &Connection) -> Result<Vec<RooflineLoop>> {
    let columns = table_columns(connection, "roofline");
    if columns.is_empty() {
        bail!("Roofline data is unavailable: table `roofline` does not exist");
    }
    let has = |name: &str| columns.iter().any(|column| column.name == name);
    let confidence_columns = if has("timing_quality") {
        "timing_samples, timing_relative_error, timing_quality, module_offset, trip_count"
    } else {
        "NULL AS timing_samples, NULL AS timing_relative_error, NULL AS timing_quality, NULL AS module_offset, NULL AS trip_count"
    };
    let thread_columns = if has("thread_count") {
        "thread_count, duration_ns, cpu_time_ns"
    } else {
        "NULL AS thread_count, NULL AS duration_ns, NULL AS cpu_time_ns"
    };
    let query = format!(
        "
        SELECT
            function_name,
            file_name,
            line,
            scalar_int_ops,
            scalar_int_ai,
            scalar_float_ops,
            scalar_float_ai,
            scalar_double_ops,
            scalar_double_ai,
            vector_int_ops,
            vector_int_ai,
            vector_float_ops,
            vector_float_ai,
            vector_double_ops,
            vector_double_ai,
            {confidence_columns},
            {thread_columns}
        FROM roofline
        ORDER BY
            COALESCE(scalar_double_ops, 0) + COALESCE(vector_double_ops, 0) DESC,
            function_name ASC,
            file_name ASC,
            line ASC;
    "
    );
    let mut statement = connection
        .prepare(&query)
        .context("Roofline data is unavailable: failed to query view `roofline`")?;
    let mut rows = statement
        .query([])
        .context("Roofline data is unavailable: failed to query view `roofline`")?;
    let mut loops = Vec::new();
    while let Some(row) = rows
        .next()
        .context("failed to read a row from view `roofline`")?
    {
        let row = row_values(row, 23).context("failed to read a row from view `roofline`")?;
        loops.push(RooflineLoop {
            function_name: string_value(&row[0]).unwrap_or_else(|| "[unknown loop]".to_string()),
            file_name: string_value(&row[1]).unwrap_or_default(),
            line: as_i64(&row[2]).unwrap_or_default().max(0) as usize,
            scalar_int_ops: finite_value(&row[3]),
            scalar_int_ai: finite_value(&row[4]),
            scalar_float_ops: finite_value(&row[5]),
            scalar_float_ai: finite_value(&row[6]),
            scalar_double_ops: finite_value(&row[7]),
            scalar_double_ai: finite_value(&row[8]),
            vector_int_ops: finite_value(&row[9]),
            vector_int_ai: finite_value(&row[10]),
            vector_float_ops: finite_value(&row[11]),
            vector_float_ai: finite_value(&row[12]),
            vector_double_ops: finite_value(&row[13]),
            vector_double_ai: finite_value(&row[14]),
            timing_samples: as_i64(&row[15]).and_then(|value| u64::try_from(value).ok()),
            timing_relative_error: finite_value(&row[16]),
            timing_quality: string_value(&row[17]),
            module_offset: string_value(&row[18]),
            trip_count: as_i64(&row[19]).and_then(|value| u64::try_from(value).ok()),
            thread_count: as_i64(&row[20]).and_then(|value| u32::try_from(value).ok()),
            duration_ns: as_i64(&row[21]).and_then(|value| u64::try_from(value).ok()),
            cpu_time_ns: as_i64(&row[22]).and_then(|value| u64::try_from(value).ok()),
        });
    }
    Ok(loops)
}

fn string_value(value: &Value) -> Option<String> {
    as_text(value).map(str::to_string)
}

fn finite_value(value: &Value) -> Option<f64> {
    as_f64(value).filter(|value| value.is_finite())
}

fn finite_positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

fn sum_optional(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mperf_data::RooflineCeilings;

    fn calibration() -> RooflineCalibration {
        RooflineCalibration {
            threads: 4,
            cpu_affinity: Some("0-3".to_string()),
            samples: 5,
            compute_kernel: "test-fma".to_string(),
            fp64_gflops: 200.0,
            fp64_gflops_samples: vec![200.0],
            memory_gbytes_per_second: 50.0,
            memory_gbytes_per_second_samples: vec![50.0],
            ridge_point_flops_per_byte: 4.0,
            memory_working_set_bytes: 1024,
            memory_levels: vec![
                MemoryLevelCalibration {
                    level: "L1".to_string(),
                    gbytes_per_second: 400.0,
                    gbytes_per_second_samples: vec![400.0],
                    working_set_bytes: 64,
                    capacity_bytes: 32 * 1024,
                    shared_by: 1,
                },
                MemoryLevelCalibration {
                    level: "DRAM".to_string(),
                    gbytes_per_second: 50.0,
                    gbytes_per_second_samples: vec![50.0],
                    working_set_bytes: 1024,
                    capacity_bytes: 0,
                    shared_by: 4,
                },
            ],
            single_thread: None,
        }
    }

    fn method(traffic: &str) -> RooflineMethodInfo {
        RooflineMethodInfo {
            selection: "auto".to_string(),
            accounting: "qemu".to_string(),
            performance: "native".to_string(),
            traffic: traffic.to_string(),
            quality: "test".to_string(),
            reason: "test".to_string(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn loads_every_roofline_row_and_combines_fp64_scalar_and_vector_values() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE roofline (
                    function_name TEXT,
                    file_name TEXT,
                    line BIGINT,
                    scalar_int_ops DOUBLE,
                    scalar_int_ai DOUBLE,
                    scalar_float_ops DOUBLE,
                    scalar_float_ai DOUBLE,
                    scalar_double_ops DOUBLE,
                    scalar_double_ai DOUBLE,
                    vector_int_ops DOUBLE,
                    vector_int_ai DOUBLE,
                    vector_float_ops DOUBLE,
                    vector_float_ai DOUBLE,
                    vector_double_ops DOUBLE,
                    vector_double_ai DOUBLE
                );
                INSERT INTO roofline VALUES
                    ('mixed', '/src/kernel.c', 42,
                     0, 0, 0, 0, 2000000000, 2,
                     0, 0, 0, 0, 6000000000, 6),
                    ('integer-only', '/src/kernel.c', 9,
                     100, 1, 0, 0, 0, 0,
                     200, 2, 0, 0, 0, 0);",
            )
            .unwrap();

        let data = RooflineData::load(&connection, Some(calibration()), None);

        assert_eq!(data.loops.len(), 2);
        assert_eq!(data.loops[0].function_name, "mixed");
        assert_eq!(data.loops[0].fp64_gflops(), Some(8.0));
        assert_eq!(data.loops[0].fp64_arithmetic_intensity(), Some(8.0));
        assert_eq!(data.loops[1].function_name, "integer-only");
        assert_eq!(data.loops[1].fp64_gflops(), None);
    }

    #[test]
    fn computes_memory_and_compute_bound_efficiency_against_selected_roofs() {
        let mut loop_data = RooflineLoop {
            function_name: "loop".to_string(),
            file_name: String::new(),
            line: 0,
            scalar_int_ops: None,
            scalar_int_ai: None,
            scalar_float_ops: None,
            scalar_float_ai: None,
            scalar_double_ops: Some(25_000_000_000.0),
            scalar_double_ai: Some(1.0),
            vector_int_ops: None,
            vector_int_ai: None,
            vector_float_ops: None,
            vector_float_ai: None,
            vector_double_ops: None,
            vector_double_ai: None,
            timing_samples: None,
            timing_relative_error: None,
            timing_quality: None,
            module_offset: None,
            trip_count: None,
            thread_count: None,
            duration_ns: None,
            cpu_time_ns: None,
        };
        let architectural = RooflineData {
            loops: Vec::new(),
            calibration: Some(calibration()),
            method: Some(method("architectural")),
        };
        let dram = RooflineData {
            loops: Vec::new(),
            calibration: Some(calibration()),
            method: Some(method("dram")),
        };

        assert_eq!(architectural.efficiency(&loop_data), Some(0.125));
        assert_eq!(dram.efficiency(&loop_data), Some(0.5));

        loop_data.scalar_double_ops = Some(100_000_000_000.0);
        loop_data.scalar_double_ai = Some(8.0);
        assert_eq!(architectural.efficiency(&loop_data), Some(0.5));
        assert_eq!(dram.efficiency(&loop_data), Some(0.5));
    }

    #[test]
    fn single_thread_loops_are_rated_against_the_single_thread_roof() {
        let mut calibration = calibration();
        calibration.single_thread = Some(RooflineCeilings {
            threads: 1,
            fp64_gflops: 50.0,
            fp64_gflops_samples: vec![50.0],
            memory_gbytes_per_second: 20.0,
            memory_gbytes_per_second_samples: vec![20.0],
            ridge_point_flops_per_byte: 2.5,
            memory_levels: vec![MemoryLevelCalibration {
                level: "DRAM".to_string(),
                gbytes_per_second: 20.0,
                gbytes_per_second_samples: vec![20.0],
                working_set_bytes: 1024,
                capacity_bytes: 0,
                shared_by: 1,
            }],
        });
        let data = RooflineData {
            loops: Vec::new(),
            calibration: Some(calibration),
            method: Some(method("dram")),
        };
        let mut loop_data = RooflineLoop {
            function_name: "loop".to_string(),
            file_name: String::new(),
            line: 0,
            scalar_int_ops: None,
            scalar_int_ai: None,
            scalar_float_ops: None,
            scalar_float_ai: None,
            scalar_double_ops: Some(25_000_000_000.0),
            scalar_double_ai: Some(8.0),
            vector_int_ops: None,
            vector_int_ai: None,
            vector_float_ops: None,
            vector_float_ai: None,
            vector_double_ops: None,
            vector_double_ai: None,
            timing_samples: None,
            timing_relative_error: None,
            timing_quality: None,
            module_offset: None,
            trip_count: None,
            thread_count: Some(1),
            duration_ns: Some(1_000),
            cpu_time_ns: Some(1_000),
        };
        assert_eq!(
            data.single_thread_roofs(),
            Some((50.0, vec![("DRAM", 20.0)]))
        );
        assert!(data.uses_single_thread_roofs(&loop_data));
        assert_eq!(data.efficiency(&loop_data), Some(0.5));

        loop_data.thread_count = Some(4);
        assert!(!data.uses_single_thread_roofs(&loop_data));
        assert_eq!(data.efficiency(&loop_data), Some(0.125));
    }

    #[test]
    fn keeps_an_empty_loop_list_when_the_roofline_table_is_missing() {
        let connection = Connection::open_in_memory().unwrap();
        let data = RooflineData::load(&connection, Some(calibration()), None);

        assert!(data.loops.is_empty());
        assert!(data.calibration.is_some());
    }

    #[test]
    fn selects_cache_hierarchy_for_carm_and_dram_roof_for_dram_intensity() {
        let connection = Connection::open_in_memory().unwrap();

        let architectural = RooflineData::load(
            &connection,
            Some(calibration()),
            Some(method("architectural")),
        );
        assert_eq!(
            architectural.bandwidth_roofs(),
            vec![("L1", 400.0), ("DRAM", 50.0)]
        );

        let dram = RooflineData::load(&connection, Some(calibration()), Some(method("dram")));
        assert_eq!(dram.bandwidth_roofs(), vec![("DRAM", 50.0)]);

        let modeled =
            RooflineData::load(&connection, Some(calibration()), Some(method("dram-model")));
        assert_eq!(modeled.bandwidth_roofs(), vec![("DRAM", 50.0)]);

        let unknown = RooflineData::load(&connection, Some(calibration()), Some(method("unknown")));
        assert!(unknown.bandwidth_roofs().is_empty());
    }

    #[test]
    fn roofline_label_asset_escapes_recorded_level_names() {
        let path = roofline_label_asset("L<&> · 1.00 GB/s");
        let svg = String::from_utf8(roofline_label_svg(&path).unwrap()).unwrap();

        assert!(svg.contains("L&lt;&amp;&gt; · 1.00 GB/s"));
        assert!(!svg.contains("L<&>"));
    }
}
