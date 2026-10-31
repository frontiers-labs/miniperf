mod allocations;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use mperf_data::{CallFrame, Event, EventType, MemSample, ProcMapEntry};
use store::{
    ClockAnchorRows, EventKind, EventRows, MemSampleRawRows, ModuleRows, PayloadRows,
    SampleRawRows, SegmentWriter, StringInterner, xxh3,
};
use thread_local::ThreadLocal;

use crate::utils::counter_to_event_ty;

/// Routes recorded events into the Parquet session directory: PMU/OS samples
/// into `samples_raw`, instrumentation events into `events`/`payloads`, and
/// module mappings into `modules`. Strings are interned with XXH3-64 IDs.
pub struct EventDispatcher {
    interner: Arc<Mutex<StringInterner>>,
    last_unique_id: ThreadLocal<RefCell<u64>>,
    tx: mpsc::SyncSender<Msg>,
}

enum Msg {
    Event(Box<Event>),
    Allocation(libprof::Allocation),
    AllocationCoverage(libprof::AllocationCoverage),
    MemSample(MemSample),
    Module(ProcMapEntry),
    Resource(libprof::ResourceSample),
    Process(libprof::ProcessInfo),
    Metric(&'static str, String, f64),
}

pub struct DispatcherJoinHandle {
    worker: std::thread::JoinHandle<anyhow::Result<()>>,
}

const BATCH_ROWS: usize = 8192;
/// Cap on the raw sample bytes held before a Parquet write.
///
/// A row's captured user stack dwarfs its scalar columns, so a row count alone
/// says nothing about the size of the write it will produce. 8192 rows of
/// stack dumps encode for tens of milliseconds, and because the sampling
/// driver publishes through a bounded channel, that stall reaches back into the
/// kernel's perf ring, which is far smaller and drops what it cannot hold.
/// Bounding the batch by bytes keeps each write short enough for the ring to
/// ride out.
const BATCH_STACK_BYTES: usize = 1024 * 1024;
/// Resource rows arrive in ~1Hz bursts. Writing one batch per burst is what
/// the collectors did before they wrote through the sink, and it is what keeps
/// a killed recording's telemetry on disk.
const RESOURCE_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// Backstop for a source that produces resource rows far faster than 1Hz.
const RESOURCE_BATCH_ROWS: usize = 4096;

struct Worker {
    allocations: Option<allocations::Writer>,
    interner: Arc<Mutex<StringInterner>>,
    samples: SampleRawRows,
    samples_writer: SegmentWriter,
    mem_samples: MemSampleRawRows,
    mem_samples_writer: SegmentWriter,
    events: EventRows,
    events_writer: SegmentWriter,
    payloads: PayloadRows,
    payload_seen: HashSet<u64>,
    payloads_writer: SegmentWriter,
    modules: HashSet<ProcMapEntry>,
    modules_writer: SegmentWriter,
    clock: ClockAnchorRows,
    clock_writer: SegmentWriter,
    resources: Vec<libprof::ResourceSample>,
    resources_writer: SegmentWriter,
    resources_flushed: std::time::Instant,
    /// Captured stack bytes buffered in `samples` and `mem_samples`.
    sample_bytes: usize,
    mem_sample_bytes: usize,
    processes: Vec<libprof::ProcessInfo>,
    metrics: Vec<(&'static str, String, f64)>,
    directory: std::path::PathBuf,
}

impl Worker {
    fn new(dir: &Path, interner: Arc<Mutex<StringInterner>>) -> Self {
        let mut clock = ClockAnchorRows::default();
        clock.push("start");
        Worker {
            allocations: Some(allocations::Writer::new(dir)),
            interner,
            samples: SampleRawRows::default(),
            samples_writer: SegmentWriter::new(dir, "samples_raw", None, SampleRawRows::schema()),
            mem_samples: MemSampleRawRows::default(),
            mem_samples_writer: SegmentWriter::new(
                dir,
                "mem_samples_raw",
                None,
                MemSampleRawRows::schema(),
            ),
            events: EventRows::default(),
            events_writer: SegmentWriter::new(dir, "events", None, EventRows::schema()),
            payloads: PayloadRows::default(),
            payload_seen: HashSet::new(),
            payloads_writer: SegmentWriter::new(dir, "payloads", None, PayloadRows::schema()),
            modules: HashSet::new(),
            modules_writer: SegmentWriter::new(dir, "modules", None, ModuleRows::schema()),
            clock,
            clock_writer: SegmentWriter::new(dir, "clock", None, ClockAnchorRows::schema()),
            resources: Vec::new(),
            // Roll small segments so a crash loses at most the open one: a
            // recording that dies still explains what the machine was doing.
            resources_writer: SegmentWriter::new(
                dir,
                "resource_samples",
                None,
                resource_sample_schema(),
            )
            .with_segment_bytes(256 * 1024),
            resources_flushed: std::time::Instant::now(),
            sample_bytes: 0,
            mem_sample_bytes: 0,
            processes: Vec::new(),
            metrics: Vec::new(),
            directory: dir.to_owned(),
        }
    }

    fn intern(&self, value: &str) -> u64 {
        self.interner.lock().unwrap().intern(value)
    }

    fn consume(&mut self, event: Event) {
        if event.ty.is_roofline() {
            self.consume_trace(event);
        } else {
            self.consume_sample(event);
        }
    }

    fn consume_sample(&mut self, event: Event) {
        let event_id = if event.ty == EventType::PmuCustom && event.name != 0 {
            event.name
        } else {
            self.intern(&event.ty.to_string())
        };
        let rows = &mut self.samples;
        rows.timestamp.push(event.timestamp as i64);
        rows.pid.push(event.process_id);
        rows.tid.push(event.thread_id);
        rows.cpu.push(event.cpu);
        rows.group_id.push(event.correlation_id);
        rows.event_id.push(event_id);
        rows.value.push(event.value as i64);
        rows.time_enabled.push(event.time_enabled);
        rows.time_running.push(event.time_running);
        rows.ip.push(
            event
                .callstack
                .first()
                .map(|frame| frame.as_ip())
                .unwrap_or(0),
        );
        rows.callchain
            .push(event.callstack.iter().map(|frame| frame.as_ip()).collect());
        rows.lbr_callchain.push(event.lbr_callstack);
        let (abi, mask, regs) = match event.user_regs {
            Some(regs) => (regs.abi, regs.mask, regs.values),
            None => (0, 0, Vec::new()),
        };
        rows.regs_abi.push(abi);
        rows.regs_mask.push(mask);
        rows.regs.push(regs);
        self.sample_bytes += event.user_stack.len();
        rows.user_stack.push(event.user_stack);
        if rows.len() >= BATCH_ROWS || self.sample_bytes >= BATCH_STACK_BYTES {
            self.flush_samples();
        }
    }

    fn consume_mem_sample(&mut self, sample: MemSample) {
        let rows = &mut self.mem_samples;
        rows.timestamp.push(sample.timestamp as i64);
        rows.pid.push(sample.process_id);
        rows.tid.push(sample.thread_id);
        rows.cpu.push(sample.cpu);
        rows.ip.push(sample.callstack.first().copied().unwrap_or(0));
        rows.data_addr.push(sample.data_addr);
        rows.latency.push(sample.latency);
        rows.data_src.push(sample.data_src);
        rows.callchain.push(sample.callstack);
        rows.lbr_callchain.push(sample.lbr_callstack);
        let (abi, mask, regs) = match sample.user_regs {
            Some(regs) => (regs.abi, regs.mask, regs.values),
            None => (0, 0, Vec::new()),
        };
        rows.regs_abi.push(abi);
        rows.regs_mask.push(mask);
        rows.regs.push(regs);
        self.mem_sample_bytes += sample.user_stack.len();
        rows.user_stack.push(sample.user_stack);
        if rows.len() >= BATCH_ROWS || self.mem_sample_bytes >= BATCH_STACK_BYTES {
            self.flush_mem_samples();
        }
    }

    fn consume_trace(&mut self, event: Event) {
        let (kind, event_id) = match event.ty {
            EventType::RooflineLoopStart => {
                let payload = event.callstack.first().map(|frame| frame.as_loc());
                (EventKind::Begin, self.payload_id(payload))
            }
            EventType::RooflineLoopEnd => (EventKind::End, 0),
            _ => (EventKind::Counter, self.intern(&event.ty.to_string())),
        };
        let rows = &mut self.events;
        rows.timestamp.push(event.timestamp as i64);
        rows.event_id.push(event_id);
        rows.instance.push(event.unique_id);
        rows.parent_id.push(event.parent_id);
        rows.flow_id.push(event.correlation_id);
        rows.kind.push(kind as u8);
        rows.pid.push(event.process_id);
        rows.tid.push(event.thread_id);
        rows.value.push(event.value as i64);
        if rows.len() >= BATCH_ROWS {
            self.flush_events();
        }
    }

    fn payload_id(&mut self, location: Option<mperf_data::Location>) -> u64 {
        let Some(location) = location else {
            return 0;
        };
        let mut bytes = [0u8; 24];
        bytes[..8].copy_from_slice(&location.function_name.to_le_bytes());
        bytes[8..16].copy_from_slice(&location.file_name.to_le_bytes());
        bytes[16..20].copy_from_slice(&location.line.to_le_bytes());
        let id = xxh3(&bytes);
        if self.payload_seen.insert(id) {
            self.payloads.event_id.push(id);
            self.payloads.name_id.push(location.function_name);
            self.payloads.function_id.push(location.function_name);
            self.payloads.file_id.push(location.file_name);
            self.payloads.line.push(location.line);
            self.payloads.column.push(0);
        }
        id
    }

    fn flush_samples(&mut self) {
        self.sample_bytes = 0;
        if self.samples.is_empty() {
            return;
        }
        let result = self
            .samples
            .to_batch()
            .and_then(|batch| self.samples_writer.write(&batch));
        if let Err(err) = result {
            eprintln!("failed to write samples: {err:#}");
        }
    }

    fn flush_mem_samples(&mut self) {
        self.mem_sample_bytes = 0;
        if self.mem_samples.is_empty() {
            return;
        }
        let result = self
            .mem_samples
            .to_batch()
            .and_then(|batch| self.mem_samples_writer.write(&batch));
        if let Err(err) = result {
            eprintln!("failed to write mem samples: {err:#}");
        }
    }

    fn consume_resource(&mut self, sample: libprof::ResourceSample) {
        self.resources.push(sample);
        if self.resources.len() >= RESOURCE_BATCH_ROWS
            || self.resources_flushed.elapsed() >= RESOURCE_FLUSH_INTERVAL
        {
            self.flush_resources();
        }
    }

    fn flush_resources(&mut self) {
        self.resources_flushed = std::time::Instant::now();
        if self.resources.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.resources);
        let result =
            resource_sample_batch(&rows).and_then(|batch| self.resources_writer.write(&batch));
        if let Err(err) = result {
            eprintln!("failed to write resource samples: {err:#}");
        }
    }

    fn flush_events(&mut self) {
        if self.events.is_empty() {
            return;
        }
        let result = self
            .events
            .to_batch()
            .and_then(|batch| self.events_writer.write(&batch));
        if let Err(err) = result {
            eprintln!("failed to write events: {err:#}");
        }
    }

    fn finish(mut self) -> anyhow::Result<()> {
        let allocation_result = self
            .allocations
            .take()
            .map(|writer| writer.finish())
            .transpose();
        self.flush_samples();
        self.flush_mem_samples();
        self.flush_events();
        self.flush_resources();
        let processes = process_batch(&self.processes);
        let metrics = metric_batches(&self.metrics);
        let mut modules = ModuleRows::default();
        for entry in std::mem::take(&mut self.modules) {
            modules.pid.push(entry.pid);
            modules.path.push(entry.filename);
            modules.build_id.push(String::new());
            modules.address.push(entry.address as u64);
            modules.size.push(entry.size as u64);
            modules.offset.push(entry.offset as u64);
        }
        self.clock.push("end");
        let steps = [
            (
                "payloads",
                write_all(self.payloads.to_batch(), &mut self.payloads_writer),
            ),
            (
                "modules",
                write_all(modules.to_batch(), &mut self.modules_writer),
            ),
            (
                "clock",
                write_all(self.clock.to_batch(), &mut self.clock_writer),
            ),
            ("samples_raw", self.samples_writer.finish().map(|_| ())),
            (
                "mem_samples_raw",
                self.mem_samples_writer.finish().map(|_| ()),
            ),
            ("events", self.events_writer.finish().map(|_| ())),
            (
                "resource_samples",
                self.resources_writer.finish().map(|_| ()),
            ),
            (
                "process_samples",
                write_table(&self.directory, "process_samples", processes),
            ),
            ("strings", self.interner.lock().unwrap().finish()),
        ];
        for (name, result) in steps {
            if let Err(err) = result {
                eprintln!("failed to write {name}: {err:#}");
            }
        }
        for (group, batch) in metrics {
            let table = format!("{group}_metrics");
            if let Err(err) = write_table(&self.directory, &table, batch) {
                eprintln!("failed to write {table}: {err:#}");
            }
        }
        allocation_result.map(|_| ())
    }
}

/// Write one complete table and close it. An empty batch writes nothing, so a
/// table only exists when the source that fills it actually ran.
fn write_table(
    directory: &Path,
    table: &str,
    batch: anyhow::Result<store::arrow::record_batch::RecordBatch>,
) -> anyhow::Result<()> {
    let batch = batch?;
    if batch.num_rows() == 0 {
        return Ok(());
    }
    let mut writer = SegmentWriter::new(directory, table, None, batch.schema());
    writer.write(&batch)?;
    writer.finish()?;
    Ok(())
}

fn write_all(
    batch: anyhow::Result<store::arrow::record_batch::RecordBatch>,
    writer: &mut SegmentWriter,
) -> anyhow::Result<()> {
    let batch = batch?;
    if batch.num_rows() > 0 {
        writer.write(&batch)?;
    }
    writer.finish()?;
    Ok(())
}

impl EventDispatcher {
    pub fn new(output_directory: &Path) -> (Arc<Self>, DispatcherJoinHandle) {
        let interner = Arc::new(Mutex::new(StringInterner::new(output_directory, None)));
        let (tx, rx) = mpsc::sync_channel::<Msg>(BATCH_ROWS);
        let mut worker = Worker::new(output_directory, interner.clone());
        let worker = std::thread::spawn(move || {
            while let Ok(message) = rx.recv() {
                match message {
                    Msg::Event(event) => worker.consume(*event),
                    Msg::Allocation(record) => worker.allocations.as_mut().unwrap().push(record),
                    Msg::AllocationCoverage(coverage) => {
                        worker.allocations.as_mut().unwrap().coverage(coverage)
                    }
                    Msg::MemSample(sample) => worker.consume_mem_sample(sample),
                    Msg::Module(entry) => {
                        worker.modules.insert(entry);
                    }
                    Msg::Resource(sample) => worker.consume_resource(sample),
                    Msg::Process(info) => worker.processes.push(info),
                    Msg::Metric(group, name, value) => worker.metrics.push((group, name, value)),
                }
            }
            worker.finish()
        });
        (
            Arc::new(EventDispatcher {
                interner,
                last_unique_id: ThreadLocal::new(),
                tx,
            }),
            DispatcherJoinHandle { worker },
        )
    }

    /// A session-unique 64-bit instance ID.
    pub fn unique_id(&self) -> u64 {
        let mut counter = self.last_unique_id.get_or(|| RefCell::new(0)).borrow_mut();
        *counter += 1;
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&std::process::id().to_le_bytes());
        bytes[4..12].copy_from_slice(&libprof::current_thread_id().to_le_bytes());
        bytes[12..20].copy_from_slice(&counter.to_le_bytes());
        xxh3(&bytes)
    }

    pub fn string_id(&self, string: &str) -> u64 {
        self.interner.lock().unwrap().intern(string)
    }

    pub async fn string_id_async(&self, string: &str) -> u64 {
        self.string_id(string)
    }

    pub fn publish_event_sync(&self, evt: Event) {
        if self.tx.send(Msg::Event(Box::new(evt))).is_err() {
            eprintln!("lost event: writer stopped");
        }
    }

    pub fn publish_mem_sample_sync(&self, sample: MemSample) {
        if self.tx.send(Msg::MemSample(sample)).is_err() {
            eprintln!("lost memory sample: writer stopped");
        }
    }

    pub fn publish_proc_map_sync(&self, map: ProcMapEntry) {
        if self.tx.try_send(Msg::Module(map)).is_err() {
            eprintln!("lost proc map entry: channel full or writer stopped");
        }
    }

    pub async fn publish_event(&self, evt: Event) {
        self.publish_event_sync(evt);
    }
}

impl DispatcherJoinHandle {
    pub async fn join(self) -> anyhow::Result<()> {
        let worker = self.worker;
        tokio::task::spawn_blocking(move || worker.join())
            .await?
            .map_err(|_| anyhow::anyhow!("recording writer panicked"))?
    }
}

/// The dispatcher is the CLI's adapter for libprof's [`libprof::Sink`]: every
/// source writes through it, and the session's tables are the only place the
/// records land.
impl libprof::Sink for EventDispatcher {
    fn record(&self, record: libprof::Record) {
        match record {
            libprof::Record::Allocation(record) => {
                let _ = self.tx.send(Msg::Allocation(record));
            }
            libprof::Record::AllocationCoverage(coverage) => {
                let _ = self.tx.send(Msg::AllocationCoverage(coverage));
            }
            libprof::Record::Sample(sample) => {
                let unique_id = self.unique_id();
                let mut callstack = smallvec::smallvec![CallFrame::IP(sample.ip)];
                callstack.extend(
                    sample
                        .callstack
                        .into_iter()
                        .filter(|address| *address != sample.ip)
                        .map(CallFrame::IP),
                );
                let name = match &sample.counter {
                    libprof::Counter::Custom(name) => self.string_id(name),
                    _ => 0,
                };
                self.publish_event_sync(Event {
                    unique_id,
                    correlation_id: sample.event_id as u64,
                    parent_id: 0,
                    ty: counter_to_event_ty(&sample.counter),
                    thread_id: sample.tid,
                    process_id: sample.pid,
                    cpu: sample.cpu,
                    time_enabled: sample.time_enabled,
                    time_running: sample.time_running,
                    value: sample.value,
                    timestamp: sample.time,
                    name,
                    callstack,
                    lbr_callstack: sample.lbr_callstack.into_vec(),
                    user_regs: sample.user_regs.map(user_regs),
                    user_stack: sample.user_stack,
                });
            }
            libprof::Record::MemSample(sample) => {
                let mut callstack = vec![sample.ip];
                callstack.extend(
                    sample
                        .callstack
                        .into_iter()
                        .filter(|address| *address != sample.ip),
                );
                self.publish_mem_sample_sync(MemSample {
                    timestamp: sample.time,
                    process_id: sample.pid,
                    thread_id: sample.tid,
                    cpu: sample.cpu,
                    data_addr: sample.data_addr,
                    latency: sample.latency,
                    data_src: sample.data_src,
                    callstack,
                    lbr_callstack: sample.lbr_callstack.into_vec(),
                    user_regs: sample.user_regs.map(user_regs),
                    user_stack: sample.user_stack,
                });
            }
            libprof::Record::ProcAddr(map) => self.publish_proc_map_sync(ProcMapEntry {
                filename: map.filename,
                address: map.addr as usize,
                size: map.len as usize,
                offset: map.pgoff as usize,
                pid: map.pid,
            }),
            libprof::Record::Resource(sample) => {
                if self.tx.send(Msg::Resource(sample)).is_err() {
                    eprintln!("lost resource sample: writer stopped");
                }
            }
            libprof::Record::Process(info) => {
                if self.tx.send(Msg::Process(info)).is_err() {
                    eprintln!("lost process record: writer stopped");
                }
            }
            libprof::Record::Metric { group, name, value } => {
                if self.tx.send(Msg::Metric(group, name, value)).is_err() {
                    eprintln!("lost metric: writer stopped");
                }
            }
        }
    }
}

fn user_regs(regs: libprof::UserRegs) -> mperf_data::UserRegs {
    mperf_data::UserRegs {
        abi: regs.abi,
        mask: regs.mask,
        values: regs.values,
    }
}

fn resource_sample_schema() -> Arc<store::arrow::datatypes::Schema> {
    use store::arrow::datatypes::{DataType, Field, Schema};
    Arc::new(Schema::new(vec![
        Field::new("timestamp_ns", DataType::Int64, false),
        Field::new("resource", DataType::Utf8, false),
        Field::new("resource_id", DataType::Utf8, false),
        Field::new("category", DataType::Utf8, false),
        Field::new("metric", DataType::Utf8, false),
        Field::new("value", DataType::Float64, false),
        Field::new("unit", DataType::Utf8, false),
        Field::new("scope", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("quality", DataType::Utf8, false),
    ]))
}

fn resource_sample_batch(
    samples: &[libprof::ResourceSample],
) -> anyhow::Result<store::arrow::record_batch::RecordBatch> {
    use store::arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray};
    let text = |field: fn(&libprof::ResourceSample) -> &str| -> ArrayRef {
        Arc::new(StringArray::from(
            samples.iter().map(field).collect::<Vec<_>>(),
        ))
    };
    Ok(store::arrow::record_batch::RecordBatch::try_new(
        resource_sample_schema(),
        vec![
            Arc::new(Int64Array::from(
                samples
                    .iter()
                    .map(|sample| sample.timestamp_ns as i64)
                    .collect::<Vec<_>>(),
            )),
            text(|sample| &sample.resource),
            text(|sample| &sample.resource_id),
            text(|sample| &sample.category),
            text(|sample| &sample.metric),
            Arc::new(Float64Array::from(
                samples
                    .iter()
                    .map(|sample| sample.value)
                    .collect::<Vec<_>>(),
            )),
            text(|sample| &sample.unit),
            text(|sample| &sample.scope),
            text(|sample| &sample.source),
            text(|sample| &sample.quality),
        ],
    )?)
}

fn process_batch(
    rows: &[libprof::ProcessInfo],
) -> anyhow::Result<store::arrow::record_batch::RecordBatch> {
    use store::arrow::array::{ArrayRef, Int64Array, StringArray};
    use store::arrow::datatypes::{DataType, Field, Schema};
    let schema = Arc::new(Schema::new(vec![
        Field::new("pid", DataType::Int64, false),
        Field::new("ppid", DataType::Int64, false),
        Field::new("start_ticks", DataType::Int64, false),
        Field::new("first_seen_ns", DataType::Int64, false),
        Field::new("last_seen_ns", DataType::Int64, false),
        Field::new("command", DataType::Utf8, false),
        Field::new("quality", DataType::Utf8, false),
    ]));
    let int = |field: fn(&libprof::ProcessInfo) -> i64| -> ArrayRef {
        Arc::new(Int64Array::from(rows.iter().map(field).collect::<Vec<_>>()))
    };
    let text = |field: fn(&libprof::ProcessInfo) -> &str| -> ArrayRef {
        Arc::new(StringArray::from(
            rows.iter().map(field).collect::<Vec<_>>(),
        ))
    };
    Ok(store::arrow::record_batch::RecordBatch::try_new(
        schema,
        vec![
            int(|row| row.pid as i64),
            int(|row| row.ppid as i64),
            int(|row| row.start_ticks as i64),
            int(|row| row.first_seen_ns as i64),
            int(|row| row.last_seen_ns as i64),
            text(|row| &row.command),
            text(|row| &row.quality),
        ],
    )?)
}

/// Scalar summary metrics, one `<group>_metrics` table per group.
fn metric_batches(
    metrics: &[(&'static str, String, f64)],
) -> Vec<(
    &'static str,
    anyhow::Result<store::arrow::record_batch::RecordBatch>,
)> {
    use store::arrow::array::{ArrayRef, Float64Array, StringArray};
    use store::arrow::datatypes::{DataType, Field, Schema};
    let schema = Arc::new(Schema::new(vec![
        Field::new("metric", DataType::Utf8, false),
        Field::new("value", DataType::Float64, false),
    ]));
    let mut groups: Vec<&'static str> = metrics.iter().map(|(group, _, _)| *group).collect();
    groups.sort_unstable();
    groups.dedup();
    groups
        .into_iter()
        .map(|group| {
            let rows = metrics.iter().filter(|(name, _, _)| *name == group);
            let names: Vec<&str> = rows.clone().map(|(_, name, _)| name.as_str()).collect();
            let values: Vec<f64> = rows.map(|(_, _, value)| *value).collect();
            let batch = store::arrow::record_batch::RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(names)) as ArrayRef,
                    Arc::new(Float64Array::from(values)),
                ],
            )
            .map_err(anyhow::Error::from);
            (group, batch)
        })
        .collect()
}
