use std::sync::Arc;

use anyhow::Result;
use arrow::array::{
    ArrayRef, BinaryBuilder, Int64Array, ListBuilder, StringArray, UInt8Array, UInt32Array,
    UInt64Array, UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;

/// Trace-event record type stored in the `type` column of `events` tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    Begin = 0,
    End = 1,
    Instant = 2,
    Counter = 3,
    Loss = 4,
}

fn uint64_list_field(name: &str) -> Field {
    Field::new(
        name,
        DataType::List(Arc::new(Field::new("item", DataType::UInt64, false))),
        false,
    )
}

macro_rules! columns {
    ($schema:expr, $($array:expr),+ $(,)?) => {
        RecordBatch::try_new($schema, vec![$(Arc::new($array) as ArrayRef),+])
    };
}

/// Column builder for `events-<pid>-<seq>.parquet`: one row per trace event.
#[derive(Default)]
pub struct EventRows {
    pub timestamp: Vec<i64>,
    pub event_id: Vec<u64>,
    pub instance: Vec<u64>,
    pub parent_id: Vec<u64>,
    pub flow_id: Vec<u64>,
    pub kind: Vec<u8>,
    pub pid: Vec<u32>,
    pub tid: Vec<u32>,
    pub value: Vec<i64>,
}

impl EventRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Int64, false),
            Field::new("event_id", DataType::UInt64, false),
            Field::new("instance", DataType::UInt64, false),
            Field::new("parent_id", DataType::UInt64, false),
            Field::new("flow_id", DataType::UInt64, false),
            Field::new("type", DataType::UInt8, false),
            Field::new("pid", DataType::UInt32, false),
            Field::new("tid", DataType::UInt32, false),
            Field::new("value", DataType::Int64, false),
        ]))
    }

    pub fn len(&self) -> usize {
        self.timestamp.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            Int64Array::from(std::mem::take(&mut self.timestamp)),
            UInt64Array::from(std::mem::take(&mut self.event_id)),
            UInt64Array::from(std::mem::take(&mut self.instance)),
            UInt64Array::from(std::mem::take(&mut self.parent_id)),
            UInt64Array::from(std::mem::take(&mut self.flow_id)),
            UInt8Array::from(std::mem::take(&mut self.kind)),
            UInt32Array::from(std::mem::take(&mut self.pid)),
            UInt32Array::from(std::mem::take(&mut self.tid)),
            Int64Array::from(std::mem::take(&mut self.value)),
        )?)
    }
}

/// Column builder for `event_meta-<pid>-<seq>.parquet`: sparse typed metadata.
#[derive(Default)]
pub struct EventMetaRows {
    pub event_instance: Vec<u64>,
    pub key_id: Vec<u64>,
    pub value_type: Vec<u8>,
    pub value_int: Vec<i64>,
    pub value_double: Vec<f64>,
    pub value_string_id: Vec<u64>,
}

impl EventMetaRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("event_instance", DataType::UInt64, false),
            Field::new("key_id", DataType::UInt64, false),
            Field::new("value_type", DataType::UInt8, false),
            Field::new("value_int", DataType::Int64, false),
            Field::new("value_double", DataType::Float64, false),
            Field::new("value_string_id", DataType::UInt64, false),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.event_instance.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            UInt64Array::from(std::mem::take(&mut self.event_instance)),
            UInt64Array::from(std::mem::take(&mut self.key_id)),
            UInt8Array::from(std::mem::take(&mut self.value_type)),
            Int64Array::from(std::mem::take(&mut self.value_int)),
            arrow::array::Float64Array::from(std::mem::take(&mut self.value_double)),
            UInt64Array::from(std::mem::take(&mut self.value_string_id)),
        )?)
    }
}

/// Column builder for `strings-<pid>.parquet`: interned dictionary.
#[derive(Default)]
pub struct StringRows {
    pub id: Vec<u64>,
    pub string: Vec<String>,
}

impl StringRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("string", DataType::Utf8, false),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.id.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            UInt64Array::from(std::mem::take(&mut self.id)),
            StringArray::from(std::mem::take(&mut self.string)),
        )?)
    }
}

/// Column builder for `payloads-<pid>.parquet`: trace-point identities.
#[derive(Default)]
pub struct PayloadRows {
    pub event_id: Vec<u64>,
    pub name_id: Vec<u64>,
    pub function_id: Vec<u64>,
    pub file_id: Vec<u64>,
    pub line: Vec<u32>,
    pub column: Vec<u32>,
}

impl PayloadRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("event_id", DataType::UInt64, false),
            Field::new("name_id", DataType::UInt64, false),
            Field::new("function_id", DataType::UInt64, false),
            Field::new("file_id", DataType::UInt64, false),
            Field::new("line", DataType::UInt32, false),
            Field::new("column", DataType::UInt32, false),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.event_id.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            UInt64Array::from(std::mem::take(&mut self.event_id)),
            UInt64Array::from(std::mem::take(&mut self.name_id)),
            UInt64Array::from(std::mem::take(&mut self.function_id)),
            UInt64Array::from(std::mem::take(&mut self.file_id)),
            UInt32Array::from(std::mem::take(&mut self.line)),
            UInt32Array::from(std::mem::take(&mut self.column)),
        )?)
    }
}

/// Column builder for `samples-<pid>-<seq>.parquet`: one row per PMU sample.
#[derive(Default)]
pub struct SampleRows {
    pub timestamp: Vec<i64>,
    pub pid: Vec<u32>,
    pub tid: Vec<u32>,
    pub cpu: Vec<u32>,
    pub group_id: Vec<u64>,
    pub event_id: Vec<u64>,
    pub value: Vec<i64>,
    pub time_enabled: Vec<u64>,
    pub time_running: Vec<u64>,
    pub ip: Vec<u64>,
    pub stack_id: Vec<u64>,
}

impl SampleRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Int64, false),
            Field::new("pid", DataType::UInt32, false),
            Field::new("tid", DataType::UInt32, false),
            Field::new("cpu", DataType::UInt32, false),
            Field::new("group_id", DataType::UInt64, false),
            Field::new("event_id", DataType::UInt64, false),
            Field::new("value", DataType::Int64, false),
            Field::new("time_enabled", DataType::UInt64, false),
            Field::new("time_running", DataType::UInt64, false),
            Field::new("ip", DataType::UInt64, false),
            Field::new("stack_id", DataType::UInt64, false),
        ]))
    }

    pub fn len(&self) -> usize {
        self.timestamp.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            Int64Array::from(std::mem::take(&mut self.timestamp)),
            UInt32Array::from(std::mem::take(&mut self.pid)),
            UInt32Array::from(std::mem::take(&mut self.tid)),
            UInt32Array::from(std::mem::take(&mut self.cpu)),
            UInt64Array::from(std::mem::take(&mut self.group_id)),
            UInt64Array::from(std::mem::take(&mut self.event_id)),
            Int64Array::from(std::mem::take(&mut self.value)),
            UInt64Array::from(std::mem::take(&mut self.time_enabled)),
            UInt64Array::from(std::mem::take(&mut self.time_running)),
            UInt64Array::from(std::mem::take(&mut self.ip)),
            UInt64Array::from(std::mem::take(&mut self.stack_id)),
        )?)
    }
}

/// Column builder for `stacks.parquet`: deduplicated call stacks, leaf first.
#[derive(Default)]
pub struct StackRows {
    pub stack_id: Vec<u64>,
    pub frames: Vec<Vec<u64>>,
}

impl StackRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("stack_id", DataType::UInt64, false),
            uint64_list_field("frames"),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.stack_id.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        let mut frames = ListBuilder::new(UInt64Builder::new()).with_field(Arc::new(Field::new(
            "item",
            DataType::UInt64,
            false,
        )));
        for stack in std::mem::take(&mut self.frames) {
            frames.values().append_slice(&stack);
            frames.append(true);
        }
        Ok(columns!(
            Self::schema(),
            UInt64Array::from(std::mem::take(&mut self.stack_id)),
            frames.finish(),
        )?)
    }
}

/// Column builder for `modules.parquet`: executable mappings per process.
#[derive(Default)]
pub struct ModuleRows {
    pub pid: Vec<u32>,
    pub path: Vec<String>,
    pub build_id: Vec<String>,
    pub address: Vec<u64>,
    pub size: Vec<u64>,
    pub offset: Vec<u64>,
}

impl ModuleRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("pid", DataType::UInt32, false),
            Field::new("path", DataType::Utf8, false),
            Field::new("build_id", DataType::Utf8, false),
            Field::new("address", DataType::UInt64, false),
            Field::new("size", DataType::UInt64, false),
            Field::new("offset", DataType::UInt64, false),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.pid.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            UInt32Array::from(std::mem::take(&mut self.pid)),
            StringArray::from(std::mem::take(&mut self.path)),
            StringArray::from(std::mem::take(&mut self.build_id)),
            UInt64Array::from(std::mem::take(&mut self.address)),
            UInt64Array::from(std::mem::take(&mut self.size)),
            UInt64Array::from(std::mem::take(&mut self.offset)),
        )?)
    }
}

/// Column builder for `clock-<pid>.parquet`: time anchors and calibration.
#[derive(Default)]
pub struct ClockAnchorRows {
    pub anchor: Vec<String>,
    pub monotonic_ns: Vec<i64>,
    pub realtime_ns: Vec<i64>,
}

impl ClockAnchorRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("anchor", DataType::Utf8, false),
            Field::new("monotonic_ns", DataType::Int64, false),
            Field::new("realtime_ns", DataType::Int64, false),
        ]))
    }

    pub fn push(&mut self, anchor: &str) {
        self.anchor.push(anchor.to_owned());
        self.monotonic_ns.push(crate::clock::monotonic_ns());
        self.realtime_ns.push(crate::clock::realtime_ns());
    }

    pub fn is_empty(&self) -> bool {
        self.anchor.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            StringArray::from(std::mem::take(&mut self.anchor)),
            Int64Array::from(std::mem::take(&mut self.monotonic_ns)),
            Int64Array::from(std::mem::take(&mut self.realtime_ns)),
        )?)
    }
}

/// Column builder for the record-time `samples_raw-<pid>-<seq>.parquet`
/// intermediate: raw callchains and unwind state, consumed and deleted by
/// postprocess when it writes the final `samples`/`stacks` tables.
#[derive(Default)]
pub struct SampleRawRows {
    pub timestamp: Vec<i64>,
    pub pid: Vec<u32>,
    pub tid: Vec<u32>,
    pub cpu: Vec<u32>,
    pub group_id: Vec<u64>,
    pub event_id: Vec<u64>,
    pub value: Vec<i64>,
    pub time_enabled: Vec<u64>,
    pub time_running: Vec<u64>,
    pub ip: Vec<u64>,
    pub callchain: Vec<Vec<u64>>,
    pub lbr_callchain: Vec<Vec<u64>>,
    pub regs_abi: Vec<u64>,
    pub regs_mask: Vec<u64>,
    pub regs: Vec<Vec<u64>>,
    pub user_stack: Vec<Vec<u8>>,
}

impl SampleRawRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Int64, false),
            Field::new("pid", DataType::UInt32, false),
            Field::new("tid", DataType::UInt32, false),
            Field::new("cpu", DataType::UInt32, false),
            Field::new("group_id", DataType::UInt64, false),
            Field::new("event_id", DataType::UInt64, false),
            Field::new("value", DataType::Int64, false),
            Field::new("time_enabled", DataType::UInt64, false),
            Field::new("time_running", DataType::UInt64, false),
            Field::new("ip", DataType::UInt64, false),
            uint64_list_field("callchain"),
            uint64_list_field("lbr_callchain"),
            Field::new("regs_abi", DataType::UInt64, false),
            Field::new("regs_mask", DataType::UInt64, false),
            uint64_list_field("regs"),
            Field::new("user_stack", DataType::Binary, false),
        ]))
    }

    pub fn len(&self) -> usize {
        self.timestamp.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        let item = Arc::new(Field::new("item", DataType::UInt64, false));
        let mut callchain = ListBuilder::new(UInt64Builder::new()).with_field(item.clone());
        for chain in std::mem::take(&mut self.callchain) {
            callchain.values().append_slice(&chain);
            callchain.append(true);
        }
        let mut lbr_callchain = ListBuilder::new(UInt64Builder::new()).with_field(item.clone());
        for chain in std::mem::take(&mut self.lbr_callchain) {
            lbr_callchain.values().append_slice(&chain);
            lbr_callchain.append(true);
        }
        let mut regs = ListBuilder::new(UInt64Builder::new()).with_field(item);
        for values in std::mem::take(&mut self.regs) {
            regs.values().append_slice(&values);
            regs.append(true);
        }
        let mut user_stack = BinaryBuilder::new();
        for bytes in std::mem::take(&mut self.user_stack) {
            user_stack.append_value(&bytes);
        }
        Ok(columns!(
            Self::schema(),
            Int64Array::from(std::mem::take(&mut self.timestamp)),
            UInt32Array::from(std::mem::take(&mut self.pid)),
            UInt32Array::from(std::mem::take(&mut self.tid)),
            UInt32Array::from(std::mem::take(&mut self.cpu)),
            UInt64Array::from(std::mem::take(&mut self.group_id)),
            UInt64Array::from(std::mem::take(&mut self.event_id)),
            Int64Array::from(std::mem::take(&mut self.value)),
            UInt64Array::from(std::mem::take(&mut self.time_enabled)),
            UInt64Array::from(std::mem::take(&mut self.time_running)),
            UInt64Array::from(std::mem::take(&mut self.ip)),
            callchain.finish(),
            lbr_callchain.finish(),
            UInt64Array::from(std::mem::take(&mut self.regs_abi)),
            UInt64Array::from(std::mem::take(&mut self.regs_mask)),
            regs.finish(),
            user_stack.finish(),
        )?)
    }
}

/// Column builder for the record-time `mem_samples_raw-<seq>.parquet`
/// intermediate: precise memory samples (PEBS/IBS/SPE) with their raw
/// callchains and unwind state, consumed and deleted by postprocess when it
/// writes the final `mem_samples` table.
#[derive(Default)]
pub struct MemSampleRawRows {
    pub timestamp: Vec<i64>,
    pub pid: Vec<u32>,
    pub tid: Vec<u32>,
    pub cpu: Vec<u32>,
    pub ip: Vec<u64>,
    pub data_addr: Vec<u64>,
    pub latency: Vec<u64>,
    pub data_src: Vec<u64>,
    pub callchain: Vec<Vec<u64>>,
    pub lbr_callchain: Vec<Vec<u64>>,
    pub regs_abi: Vec<u64>,
    pub regs_mask: Vec<u64>,
    pub regs: Vec<Vec<u64>>,
    pub user_stack: Vec<Vec<u8>>,
}

impl MemSampleRawRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Int64, false),
            Field::new("pid", DataType::UInt32, false),
            Field::new("tid", DataType::UInt32, false),
            Field::new("cpu", DataType::UInt32, false),
            Field::new("ip", DataType::UInt64, false),
            Field::new("data_addr", DataType::UInt64, false),
            Field::new("latency", DataType::UInt64, false),
            Field::new("data_src", DataType::UInt64, false),
            uint64_list_field("callchain"),
            uint64_list_field("lbr_callchain"),
            Field::new("regs_abi", DataType::UInt64, false),
            Field::new("regs_mask", DataType::UInt64, false),
            uint64_list_field("regs"),
            Field::new("user_stack", DataType::Binary, false),
        ]))
    }

    pub fn len(&self) -> usize {
        self.timestamp.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        let item = Arc::new(Field::new("item", DataType::UInt64, false));
        let mut callchain = ListBuilder::new(UInt64Builder::new()).with_field(item.clone());
        for chain in std::mem::take(&mut self.callchain) {
            callchain.values().append_slice(&chain);
            callchain.append(true);
        }
        let mut lbr_callchain = ListBuilder::new(UInt64Builder::new()).with_field(item.clone());
        for chain in std::mem::take(&mut self.lbr_callchain) {
            lbr_callchain.values().append_slice(&chain);
            lbr_callchain.append(true);
        }
        let mut regs = ListBuilder::new(UInt64Builder::new()).with_field(item);
        for values in std::mem::take(&mut self.regs) {
            regs.values().append_slice(&values);
            regs.append(true);
        }
        let mut user_stack = BinaryBuilder::new();
        for bytes in std::mem::take(&mut self.user_stack) {
            user_stack.append_value(&bytes);
        }
        Ok(columns!(
            Self::schema(),
            Int64Array::from(std::mem::take(&mut self.timestamp)),
            UInt32Array::from(std::mem::take(&mut self.pid)),
            UInt32Array::from(std::mem::take(&mut self.tid)),
            UInt32Array::from(std::mem::take(&mut self.cpu)),
            UInt64Array::from(std::mem::take(&mut self.ip)),
            UInt64Array::from(std::mem::take(&mut self.data_addr)),
            UInt64Array::from(std::mem::take(&mut self.latency)),
            UInt64Array::from(std::mem::take(&mut self.data_src)),
            callchain.finish(),
            lbr_callchain.finish(),
            UInt64Array::from(std::mem::take(&mut self.regs_abi)),
            UInt64Array::from(std::mem::take(&mut self.regs_mask)),
            regs.finish(),
            user_stack.finish(),
        )?)
    }
}

/// Column builder for `clock_sync-<pid>.parquet`: cross-node offset exchange
/// results (e.g. the MPI ping-pong at Init/Finalize).
#[derive(Default)]
pub struct ClockSyncRows {
    pub peer: Vec<u32>,
    pub phase: Vec<String>,
    pub local_ns: Vec<i64>,
    pub peer_ns: Vec<i64>,
    pub uncertainty_ns: Vec<i64>,
}

impl ClockSyncRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("peer", DataType::UInt32, false),
            Field::new("phase", DataType::Utf8, false),
            Field::new("local_ns", DataType::Int64, false),
            Field::new("peer_ns", DataType::Int64, false),
            Field::new("uncertainty_ns", DataType::Int64, false),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.peer.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            UInt32Array::from(std::mem::take(&mut self.peer)),
            StringArray::from(std::mem::take(&mut self.phase)),
            Int64Array::from(std::mem::take(&mut self.local_ns)),
            Int64Array::from(std::mem::take(&mut self.peer_ns)),
            Int64Array::from(std::mem::take(&mut self.uncertainty_ns)),
        )?)
    }
}

/// Column builder for `device_clock-<pid>.parquet`: bracketed calibration
/// pairs for a device timeline; post-mortem a linear fit (offset + drift)
/// maps device timestamps into CLOCK_MONOTONIC.
#[derive(Default)]
pub struct DeviceClockRows {
    pub device: Vec<String>,
    pub host_before_ns: Vec<i64>,
    pub device_ns: Vec<i64>,
    pub host_after_ns: Vec<i64>,
}

impl DeviceClockRows {
    pub fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("device", DataType::Utf8, false),
            Field::new("host_before_ns", DataType::Int64, false),
            Field::new("device_ns", DataType::Int64, false),
            Field::new("host_after_ns", DataType::Int64, false),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.device.is_empty()
    }

    pub fn to_batch(&mut self) -> Result<RecordBatch> {
        Ok(columns!(
            Self::schema(),
            StringArray::from(std::mem::take(&mut self.device)),
            Int64Array::from(std::mem::take(&mut self.host_before_ns)),
            Int64Array::from(std::mem::take(&mut self.device_ns)),
            Int64Array::from(std::mem::take(&mut self.host_after_ns)),
        )?)
    }
}

/// Least-squares linear fit mapping device time to host CLOCK_MONOTONIC from
/// bracketed calibration pairs (midpoint pairing). Returns `(offset_ns,
/// drift)` such that `host = offset_ns + drift * device`; None with fewer
/// than two pairs.
pub fn fit_device_clock(pairs: &DeviceClockRows) -> Option<(f64, f64)> {
    let n = pairs.device_ns.len();
    if n < 2 {
        return None;
    }
    let host: Vec<f64> = pairs
        .host_before_ns
        .iter()
        .zip(&pairs.host_after_ns)
        .map(|(before, after)| (*before as f64 + *after as f64) / 2.0)
        .collect();
    let device: Vec<f64> = pairs.device_ns.iter().map(|value| *value as f64).collect();
    let mean_x = device.iter().sum::<f64>() / n as f64;
    let mean_y = host.iter().sum::<f64>() / n as f64;
    let mut covariance = 0.0;
    let mut variance = 0.0;
    for (x, y) in device.iter().zip(&host) {
        covariance += (x - mean_x) * (y - mean_y);
        variance += (x - mean_x) * (x - mean_x);
    }
    if variance == 0.0 {
        return None;
    }
    let drift = covariance / variance;
    Some((mean_y - drift * mean_x, drift))
}
