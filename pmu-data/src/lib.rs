#![deny(missing_docs)]
//! Serializable schema and stable identifiers for CPU PMU event tables.

/// Arithmetic expression parser used by TMA scenario formulas.
pub mod arith_parser;

use std::collections::{BTreeSet, HashMap};

use serde::{de, Deserialize, Serialize};

// Well-known CPU families
/// AMD Zen 1 family identifier.
pub const AMDZEN1: &str = "zen1";
/// AMD Zen 2 family identifier.
pub const AMDZEN2: &str = "zen2";
/// AMD Zen 3 family identifier.
pub const AMDZEN3: &str = "zen3";
/// AMD Zen 4 family identifier.
pub const AMDZEN4: &str = "zen4";
/// AMD Zen 5 family identifier.
pub const AMDZEN5: &str = "zen5";
/// AMD Zen 6 family identifier.
pub const AMDZEN6: &str = "zen6";

/// Intel Haswell family identifier.
pub const INTEL_HASWELL: &str = "haswell";
/// Intel Broadwell family identifier.
pub const INTEL_BROADWELL: &str = "broadwell";
/// Intel Skylake family identifier.
pub const INTEL_SKYLAKE: &str = "skylake";
/// Intel Kaby Lake family identifier.
pub const INTEL_KABYLAKE: &str = "kabylake";
/// Intel Comet Lake family identifier.
pub const INTEL_COMETLAKE: &str = "cometlake";
/// Intel Ice Lake client family identifier.
pub const INTEL_ICELAKE: &str = "icelake";
// a.k.a. Ice Lake Server
/// Intel Ice Lake server family identifier.
pub const INTEL_ICX: &str = "icx";
/// Intel Tiger Lake family identifier.
pub const INTEL_TIGERLAKE: &str = "tigerlake";
/// Intel Rocket Lake family identifier.
pub const INTEL_ROCKETLAKE: &str = "rocketlake";
/// Intel Alder Lake family identifier.
pub const INTEL_ALDERLAKE: &str = "alderlake";
/// Intel Raptor Lake family identifier.
pub const INTEL_RAPTORLAKE: &str = "raptorlake";

/// SiFive U7 family identifier.
pub const SIFIVE_U7: &str = "sifive_u7";

/// SpacemiT X60 family identifier.
pub const SPACEMIT_X60: &str = "spacemit_x60";

/// SpacemiT X100 family identifier.
pub const SPACEMIT_X100: &str = "spacemit_x100";

/// SpacemiT A100 family identifier.
pub const SPACEMIT_A100: &str = "spacemit_a100";

/// Arm Cortex-A520 family identifier.
pub const ARM_CORTEX_A520: &str = "cortex_a520";
/// Arm Cortex-A720 family identifier.
pub const ARM_CORTEX_A720: &str = "cortex_a720";

/// Description of one CPU family and its supported PMU events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlatformDesc {
    /// Stable family identifier.
    pub family_id: String,
    /// Human-readable processor-family name.
    pub name: String,
    /// CPU vendor name.
    pub vendor: String,
    /// Rust target architecture containing this PMU.
    pub arch: String,
    /// Events one sampling group can schedule beyond cycles and instructions.
    pub max_counters: Option<usize>,
    /// Optional event that must lead sampling groups on this PMU.
    pub leader_event: Option<String>,
    /// Events supported by this PMU.
    pub events: Vec<EventDesc>,
    /// Portable-counter aliases provided by this PMU.
    pub aliases: Option<Vec<Alias>>,
    /// Derived metrics defined for this PMU.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<Metric>,
    /// Top-down analysis scenarios defined for this PMU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenarios: Option<Vec<TmaScenario>>,
}

/// Description and raw encoding of one PMU event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventDesc {
    /// Perf-style event name.
    pub name: String,
    /// Human-readable event description.
    pub desc: String,
    /// Raw event encoding passed to perf.
    #[serde(serialize_with = "serialize_hex", deserialize_with = "deserialize_hex")]
    pub code: u64,
}

/// Maps a portable event name to a platform-specific event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alias {
    /// Portable event name such as `cycles`.
    pub target: String,
    /// Platform-specific event name present in [`PlatformDesc::events`].
    pub origin: String,
}

/// A top-down analysis scenario and the counters needed to evaluate it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TmaScenario {
    /// Stable scenario name, normally `tma`.
    pub name: String,
    /// PMU event names that must be sampled.
    pub events: Vec<String>,
    /// Counter groups which must be scheduled together.  A metric may only
    /// use events from its group, so each result has one time-enabled/running
    /// domain instead of mixing independently multiplexed counters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<TmaGroup>,
    /// Whether this methodology has a precise (PEBS/SPE) sampling event that
    /// makes function-level ratios meaningful.
    #[serde(default)]
    pub precise_attribution: bool,
    /// Named processor constants referenced by metric formulas.
    pub constants: Vec<TmaConstant>,
    /// Hierarchical metrics calculated by the scenario.
    pub metrics: Vec<TmaMetric>,
    /// Optional TUI layout for presenting this scenario.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<ScenarioUi>,
}

/// A metric formula belonging to a top-down analysis scenario.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TmaMetric {
    /// Stable hierarchical metric name.
    pub name: String,
    /// Human-readable metric description.
    pub desc: String,
    /// Formula referencing events, constants, or other TMA metrics.
    pub formula: String,
    /// Name of the coherent counter group used to evaluate this metric.
    /// Missing is accepted for legacy scenario files and is inferred from the
    /// formula when possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// CPUs this metric is computed from, as a sysfs cpumask (`0,5-11`). Set on
    /// heterogeneous hosts, where each core type has its own topdown
    /// parameters; `None` means every CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
}

/// A set of events that must be read as one perf group.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TmaGroup {
    /// Stable group name referenced by [`TmaMetric::group`].
    pub name: String,
    /// Events in the group. `cycles` and `instructions` must be listed when a
    /// formula uses them; they are intentionally not injected implicitly.
    pub events: Vec<String>,
    /// CPUs whose PMU schedules this group, as a sysfs cpumask (`0,5-11`). Set
    /// on heterogeneous hosts for a group that belongs to one core type's
    /// formulas; `None` means every CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
}

impl TmaScenario {
    /// This scenario as one core cluster of a heterogeneous host evaluates it.
    ///
    /// Each core type has its own formulas, so two clusters' scenarios cannot
    /// share names: groups, metrics and constants get a `_cluster` suffix, and
    /// formulas are rewritten to match. A metric name is a dotted path through
    /// the hierarchy, so every segment is suffixed and a metric keeps its
    /// depth and its parent. Groups and metrics are restricted to `cpus`, the
    /// cluster's sysfs cpumask.
    pub fn for_cluster(&self, cluster: &str, cpus: &str) -> TmaScenario {
        let suffixed = |name: &str| {
            name.split('.')
                .map(|segment| format!("{segment}_{cluster}"))
                .collect::<Vec<_>>()
                .join(".")
        };
        let rename = |token: &str| match token.strip_prefix('$') {
            Some(constant) if self.constants.iter().any(|c| c.name == constant) => {
                format!("${}", suffixed(constant))
            }
            None if self.metrics.iter().any(|metric| metric.name == token) => suffixed(token),
            _ => token.to_owned(),
        };
        let rewrite = |formula: &str| {
            let mut rewritten = String::with_capacity(formula.len());
            let mut rest = formula;
            while let Some(first) = rest.chars().next() {
                // Identifiers, `$constants` and numbers share their tail
                // characters, so a number is consumed whole and never matched.
                let end = if first.is_ascii_alphanumeric() || matches!(first, '_' | '$' | '.') {
                    rest[1..]
                        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.')))
                        .map_or(rest.len(), |end| end + 1)
                } else {
                    first.len_utf8()
                };
                rewritten.push_str(&rename(&rest[..end]));
                rest = &rest[end..];
            }
            rewritten
        };

        TmaScenario {
            name: self.name.clone(),
            events: self.events.clone(),
            groups: self
                .groups
                .iter()
                .map(|group| TmaGroup {
                    name: suffixed(&group.name),
                    events: group.events.clone(),
                    cpus: Some(cpus.to_owned()),
                })
                .collect(),
            precise_attribution: self.precise_attribution,
            constants: self
                .constants
                .iter()
                .map(|constant| TmaConstant {
                    name: suffixed(&constant.name),
                    value: constant.value,
                })
                .collect(),
            metrics: self
                .metrics
                .iter()
                .map(|metric| TmaMetric {
                    name: suffixed(&metric.name),
                    desc: metric.desc.clone(),
                    formula: rewrite(&metric.formula),
                    group: metric.group.as_deref().map(suffixed),
                    cpus: Some(cpus.to_owned()),
                })
                .collect(),
            ui: None,
        }
    }

    /// Joins per-cluster scenarios into the one a heterogeneous host records.
    /// `None` when there is nothing to join.
    pub fn merge(clusters: impl IntoIterator<Item = TmaScenario>) -> Option<TmaScenario> {
        clusters.into_iter().reduce(|mut merged, cluster| {
            for event in cluster.events {
                if !merged.events.contains(&event) {
                    merged.events.push(event);
                }
            }
            merged.groups.extend(cluster.groups);
            merged.precise_attribution &= cluster.precise_attribution;
            merged.constants.extend(cluster.constants);
            merged.metrics.extend(cluster.metrics);
            merged.ui = None;
            merged
        })
    }
}

/// A named integer constant referenced by a TMA formula.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TmaConstant {
    /// Constant name without the `$` prefix.
    pub name: String,
    /// Constant value.
    pub value: u32,
}

/// Declarative TUI layout for a profiling scenario.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenarioUi {
    /// Tabs displayed for this scenario, in order.
    #[serde(default)]
    pub tabs: Vec<TabSpec>,
}

/// One tab in a declarative scenario UI.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabSpec {
    /// Recording summary.
    Summary,
    /// Linux USE-method resource overview.
    Resources,
    /// Sample flamegraph.
    Flamegraph,
    /// Instrumented loop statistics.
    Loops,
    /// Process-wide memory footprint, locality, and bandwidth analysis.
    Memory,
    /// Configurable table backed by a SQLite view.
    MetricsTable(MetricsTableSpec),
}

/// Configuration for a table backed by a SQLite view.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricsTableSpec {
    /// SQLite view queried by the tab.
    pub view: String,
    /// Optional tab title.
    #[serde(default)]
    pub title: Option<String>,
    /// Whether standard function, share, cycle, instruction, and IPC columns are included.
    #[serde(default = "default_true")]
    pub include_default_columns: bool,
    /// Additional columns read from the view.
    #[serde(default)]
    pub columns: Vec<MetricColumnSpec>,
    /// Default row ordering.
    #[serde(default)]
    pub order_by: Option<OrderSpec>,
    /// Optional maximum number of rows.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Number of leading columns kept visible during horizontal scrolling.
    #[serde(default)]
    pub sticky_columns: Option<usize>,
    /// Column used to identify a function for assembly display.
    #[serde(default)]
    pub function_column: Option<String>,
    /// Whether assembly drill-down is enabled.
    #[serde(default)]
    pub enable_assembly: bool,
}

/// Default ordering for a metrics table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderSpec {
    /// Column name used for ordering.
    pub column: String,
    /// Sort direction.
    #[serde(default)]
    pub direction: SortDirection,
}

/// Sort direction for a metrics table.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    /// Ascending order.
    Asc,
    /// Descending order.
    #[default]
    Desc,
}

/// Description of one configurable metrics-table column.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricColumnSpec {
    /// SQLite column name.
    pub key: String,
    /// Optional display label.
    #[serde(default)]
    pub label: Option<String>,
    /// Value formatting style.
    #[serde(default)]
    pub format: ValueFormat,
    /// Optional display width.
    #[serde(default)]
    pub width: Option<u16>,
    /// Whether the column remains visible while scrolling horizontally.
    #[serde(default)]
    pub sticky: bool,
    /// Whether a missing SQLite column may be omitted.
    #[serde(default)]
    pub optional: bool,
}

/// Display formatting for a metrics-table value.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValueFormat {
    /// Infer formatting from the SQLite value.
    #[default]
    Auto,
    /// Display as text.
    Text,
    /// Display as an integer.
    Integer,
    /// Display as a floating-point value with default precision.
    Float,
    /// Display a floating-point value with one decimal place.
    Float1,
    /// Display a floating-point value with two decimal places.
    Float2,
    /// Display a floating-point value with three decimal places.
    Float3,
    /// Display a percentage with default precision.
    Percent,
    /// Display a percentage with one decimal place.
    Percent1,
    /// Display a percentage with two decimal places.
    Percent2,
    /// Display a percentage with three decimal places.
    Percent3,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// A named metric derived from one or more PMU events.
pub struct Metric {
    /// Stable metric name.
    pub name: String,
    /// Human-readable metric description.
    pub desc: String,
    /// Arithmetic expression evaluated from event values.
    pub expression: MetricExpression,
    /// Optional display unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// Arithmetic metric expression containing event names and numeric literals.
pub struct MetricExpression(pub String);

#[derive(Clone, Debug, PartialEq)]
/// Error produced while parsing or evaluating a metric expression.
pub enum MetricError {
    /// An unsupported token was encountered.
    UnexpectedToken {
        /// Byte offset of the token.
        offset: usize,
        /// Unsupported character.
        token: char,
    },
    /// The expression ended while another operand was required.
    UnexpectedEnd,
    /// An opening parenthesis was not closed.
    MissingClosingParenthesis {
        /// Byte offset of the opening expression.
        offset: usize,
    },
    /// The expression references an event absent from the input values.
    UnknownEvent(String),
    /// The expression attempted division by zero.
    DivisionByZero,
    /// Evaluation produced infinity or NaN.
    NonFiniteResult,
}

impl std::fmt::Display for MetricError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedToken { offset, token } => {
                write!(formatter, "unexpected token '{token}' at byte {offset}")
            }
            Self::UnexpectedEnd => formatter.write_str("unexpected end of metric expression"),
            Self::MissingClosingParenthesis { offset } => {
                write!(formatter, "missing closing parenthesis at byte {offset}")
            }
            Self::UnknownEvent(name) => {
                write!(formatter, "metric references unknown event '{name}'")
            }
            Self::DivisionByZero => formatter.write_str("metric expression divided by zero"),
            Self::NonFiniteResult => {
                formatter.write_str("metric expression produced a non-finite result")
            }
        }
    }
}

impl std::error::Error for MetricError {}

impl MetricExpression {
    /// Evaluates the expression using event values keyed by event name.
    pub fn evaluate(&self, values: &HashMap<String, f64>) -> Result<f64, MetricError> {
        let mut parser = ExpressionParser::new(&self.0, Some(values));
        let value = parser.expression()?;
        parser.finish()?;
        if value.is_finite() {
            Ok(value)
        } else {
            Err(MetricError::NonFiniteResult)
        }
    }

    /// Returns every event name referenced by this expression.
    pub fn event_names(&self) -> Result<BTreeSet<String>, MetricError> {
        let mut parser = ExpressionParser::new(&self.0, None);
        parser.expression()?;
        parser.finish()?;
        Ok(parser.names)
    }
}

struct ExpressionParser<'a> {
    input: &'a str,
    offset: usize,
    values: Option<&'a HashMap<String, f64>>,
    names: BTreeSet<String>,
}

impl<'a> ExpressionParser<'a> {
    fn new(input: &'a str, values: Option<&'a HashMap<String, f64>>) -> Self {
        Self {
            input,
            offset: 0,
            values,
            names: BTreeSet::new(),
        }
    }

    fn expression(&mut self) -> Result<f64, MetricError> {
        let mut value = self.term()?;
        loop {
            self.whitespace();
            if self.consume('+') {
                value += self.term()?;
            } else if self.consume('-') {
                value -= self.term()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn term(&mut self) -> Result<f64, MetricError> {
        let mut value = self.factor()?;
        loop {
            self.whitespace();
            if self.consume('*') {
                value *= self.factor()?;
            } else if self.consume('/') {
                let divisor = self.factor()?;
                if divisor == 0.0 && self.values.is_some() {
                    return Err(MetricError::DivisionByZero);
                }
                value /= divisor;
            } else {
                return Ok(value);
            }
        }
    }

    fn factor(&mut self) -> Result<f64, MetricError> {
        self.whitespace();
        if self.consume('-') {
            return Ok(-self.factor()?);
        }
        if self.consume('+') {
            return self.factor();
        }
        if self.consume('(') {
            let value = self.expression()?;
            self.whitespace();
            if !self.consume(')') {
                return Err(MetricError::MissingClosingParenthesis {
                    offset: self.offset,
                });
            }
            return Ok(value);
        }
        let Some(next) = self.peek() else {
            return Err(MetricError::UnexpectedEnd);
        };
        if next.is_ascii_digit() || next == '.' {
            return self.number();
        }
        if is_identifier_char(next) {
            return self.identifier();
        }
        Err(MetricError::UnexpectedToken {
            offset: self.offset,
            token: next,
        })
    }

    fn number(&mut self) -> Result<f64, MetricError> {
        let start = self.offset;
        while self
            .peek()
            .is_some_and(|ch| ch.is_ascii_digit() || matches!(ch, '.' | 'e' | 'E' | '+' | '-'))
        {
            let ch = self.peek().expect("peeked character");
            if matches!(ch, '+' | '-') && self.offset > start {
                let previous = self.input.as_bytes()[self.offset - 1] as char;
                if !matches!(previous, 'e' | 'E') {
                    break;
                }
            }
            self.bump();
        }
        self.input[start..self.offset]
            .parse()
            .map_err(|_| MetricError::UnexpectedToken {
                offset: start,
                token: self.input[start..].chars().next().unwrap_or('?'),
            })
    }

    fn identifier(&mut self) -> Result<f64, MetricError> {
        let start = self.offset;
        while self.peek().is_some_and(is_identifier_char) {
            self.bump();
        }
        let name = self.input[start..self.offset].to_owned();
        self.names.insert(name.clone());
        let Some(values) = self.values else {
            return Ok(0.0);
        };
        values
            .get(&name)
            .copied()
            .or_else(|| {
                values
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(&name))
                    .map(|(_, value)| *value)
            })
            .ok_or(MetricError::UnknownEvent(name))
    }

    fn finish(&mut self) -> Result<(), MetricError> {
        self.whitespace();
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(MetricError::UnexpectedToken {
                offset: self.offset,
                token,
            }),
        }
    }

    fn whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.offset..].chars().next()
    }

    fn bump(&mut self) {
        if let Some(ch) = self.peek() {
            self.offset += ch.len_utf8();
        }
    }
}

fn is_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '#' | '@' | ':')
}

fn serialize_hex<S>(v: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let string = format!("0x{:X}", v);
    serializer.serialize_str(&string)
}

fn deserialize_hex<'a, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'a>,
{
    struct Visitor;

    impl de::Visitor<'_> for Visitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string containing a hexadecimal number starting with '0x'")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !v.starts_with("0x") {
                return Err(E::custom("does not start with '0x'"));
            }

            let hex_only = &v[2..];
            match u64::from_str_radix(hex_only, 16) {
                Ok(value) => Ok(value),
                Err(err) => Err(E::custom(err)),
            }
        }
    }

    deserializer.deserialize_str(Visitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_level_scenario() -> TmaScenario {
        TmaScenario {
            name: "tma".to_owned(),
            events: vec!["cycles".to_owned(), "stall.fe".to_owned()],
            groups: vec![TmaGroup {
                name: "fe".to_owned(),
                events: vec!["cycles".to_owned(), "stall.fe".to_owned()],
                cpus: None,
            }],
            precise_attribution: true,
            constants: vec![TmaConstant {
                name: "width".to_owned(),
                value: 4,
            }],
            metrics: vec![
                TmaMetric {
                    name: "fe_bound".to_owned(),
                    desc: String::new(),
                    formula: "stall.fe / ($width * cycles)".to_owned(),
                    group: Some("fe".to_owned()),
                    cpus: None,
                },
                TmaMetric {
                    name: "fe_bound.rest".to_owned(),
                    desc: String::new(),
                    formula: "1.5 - fe_bound * 2".to_owned(),
                    group: Some("fe".to_owned()),
                    cpus: None,
                },
            ],
            ui: None,
        }
    }

    #[test]
    fn a_cluster_scenario_renames_what_another_cluster_could_also_define() {
        let big = two_level_scenario().for_cluster("big", "0-1,6-11");

        assert_eq!(big.groups[0].name, "fe_big");
        assert_eq!(big.groups[0].cpus.as_deref(), Some("0-1,6-11"));
        assert_eq!(big.constants[0].name, "width_big");
        assert_eq!(big.metrics[0].name, "fe_bound_big");
        assert_eq!(big.metrics[0].group.as_deref(), Some("fe_big"));
        assert_eq!(big.metrics[0].cpus.as_deref(), Some("0-1,6-11"));
        // Events keep their names: they are columns of the recording.
        assert_eq!(big.metrics[0].formula, "stall.fe / ($width_big * cycles)");
        // A child keeps its depth and its parent.
        assert_eq!(big.metrics[1].name, "fe_bound_big.rest_big");
        // A metric reference follows the rename; a number is left alone.
        assert_eq!(big.metrics[1].formula, "1.5 - fe_bound_big * 2");
    }

    #[test]
    fn merged_clusters_keep_their_own_formulas_and_share_events() {
        let mut little = two_level_scenario();
        little.events.push("stall.be".to_owned());
        little.constants[0].value = 3;
        little.precise_attribution = false;
        let merged = TmaScenario::merge([
            two_level_scenario().for_cluster("big", "0-1"),
            little.for_cluster("little", "2-5"),
        ])
        .unwrap();

        assert_eq!(merged.events, ["cycles", "stall.fe", "stall.be"]);
        assert_eq!(
            merged
                .constants
                .iter()
                .map(|constant| (constant.name.as_str(), constant.value))
                .collect::<Vec<_>>(),
            [("width_big", 4), ("width_little", 3)]
        );
        assert_eq!(
            merged
                .metrics
                .iter()
                .map(|metric| metric.name.as_str())
                .collect::<Vec<_>>(),
            [
                "fe_bound_big",
                "fe_bound_big.rest_big",
                "fe_bound_little",
                "fe_bound_little.rest_little"
            ]
        );
        assert!(!merged.precise_attribution);
    }

    #[test]
    fn evaluates_metric_expression_with_precedence_and_parentheses() {
        let values = HashMap::from([
            ("instructions".to_owned(), 2_400.0),
            ("cycles".to_owned(), 1_200.0),
            ("misses".to_owned(), 12.0),
        ]);
        let expression = MetricExpression("instructions / cycles + misses * (2 - 1)".to_owned());
        assert_eq!(expression.evaluate(&values).unwrap(), 14.0);
        assert_eq!(
            expression.event_names().unwrap(),
            BTreeSet::from([
                "cycles".to_owned(),
                "instructions".to_owned(),
                "misses".to_owned()
            ])
        );
    }

    #[test]
    fn reports_metric_expression_errors() {
        let values = HashMap::from([("cycles".to_owned(), 0.0)]);
        assert_eq!(
            MetricExpression("instructions / cycles".to_owned()).evaluate(&values),
            Err(MetricError::UnknownEvent("instructions".to_owned()))
        );
        let values = HashMap::from([("instructions".to_owned(), 1.0), ("cycles".to_owned(), 0.0)]);
        assert_eq!(
            MetricExpression("instructions / cycles".to_owned()).evaluate(&values),
            Err(MetricError::DivisionByZero)
        );
        assert!(matches!(
            MetricExpression("(cycles + 1".to_owned()).evaluate(&values),
            Err(MetricError::MissingClosingParenthesis { .. })
        ));
        assert!(matches!(
            MetricExpression("cycles ? 1".to_owned()).evaluate(&values),
            Err(MetricError::UnexpectedToken { token: '?', .. })
        ));
    }
}
