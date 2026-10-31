use libprof::Allocation;
use std::{collections::HashMap, path::Path};
use store::Columns;
use store::{SegmentWriter, arrow::record_batch::RecordBatch};

pub(super) struct Writer {
    operations: Vec<(Allocation, u64)>,
    failure: Option<String>,
    stacks: HashMap<(u32, Vec<u64>), u64>,
    coverage: Vec<libprof::AllocationCoverage>,
    writer: SegmentWriter,
    directory: std::path::PathBuf,
}
impl Writer {
    pub fn new(directory: &Path) -> Self {
        Self {
            operations: Vec::new(),
            failure: None,
            stacks: HashMap::new(),
            coverage: Vec::new(),
            writer: SegmentWriter::new(
                directory,
                "allocation_operations",
                None,
                operations_batch(&[]).unwrap().schema(),
            ),
            directory: directory.to_owned(),
        }
    }
    pub fn push(&mut self, record: Allocation) {
        let stack_id = if record.attribution.frames().is_empty() {
            0
        } else {
            let next = self.stacks.len() as u64 + 1;
            *self
                .stacks
                .entry((record.pid, record.attribution.frames().to_vec()))
                .or_insert(next)
        };
        self.operations.push((record, stack_id));
        if self.operations.len() >= 8192 {
            self.flush();
        }
    }
    pub fn coverage(&mut self, coverage: libprof::AllocationCoverage) {
        self.coverage.push(coverage);
    }
    fn flush(&mut self) {
        if self.operations.is_empty() {
            return;
        }
        let result = operations_batch(&self.operations).and_then(|batch| self.writer.write(&batch));
        if let Err(error) = result {
            self.failure = Some(format!("{error:#}"));
        }
        self.operations.clear();
    }
    pub fn finish(mut self) -> anyhow::Result<()> {
        self.flush();
        let segments = self.writer.finish()?;
        if let Some(error) = self.failure {
            anyhow::bail!("allocation storage failed: {error}");
        }
        if self.coverage.is_empty() {
            return Ok(());
        }
        if segments.is_empty() {
            store::write_table(
                &self.directory,
                "allocation_operations",
                &[operations_batch(&[])?],
            )?;
        }
        let mut columns = Columns::default();
        macro_rules! u {
            ($field:ident) => {
                columns.u64(
                    stringify!($field),
                    self.coverage.iter().map(|r| r.$field).collect(),
                );
            };
        }
        u!(pid);
        u!(produced);
        u!(observed_operations);
        u!(producer_dropped);
        u!(channel_dropped_operations);
        u!(channel_dropped_packets);
        u!(paused_operations);
        u!(stack_every);
        columns.i64(
            "complete",
            self.coverage
                .iter()
                .map(|r| i64::from(r.complete))
                .collect(),
        );
        columns.i64(
            "footer",
            self.coverage.iter().map(|r| i64::from(r.footer)).collect(),
        );
        columns.i64(
            "invalid_protocol",
            self.coverage
                .iter()
                .map(|r| i64::from(r.invalid_protocol))
                .collect(),
        );
        columns.text(
            "scope",
            self.coverage
                .iter()
                .map(|_| "root_process_libc_interposable".into())
                .collect(),
        );
        store::write_table(&self.directory, "allocation_capture", &[columns.finish()?])?;
        let mut columns = Columns::default();
        let rows = self
            .stacks
            .into_iter()
            .flat_map(|((pid, frames), stack)| {
                frames
                    .into_iter()
                    .enumerate()
                    .map(move |(index, pc)| (pid, stack, index, pc))
            })
            .collect::<Vec<_>>();
        columns.u64("pid", rows.iter().map(|r| r.0 as u64).collect());
        columns.u64("stack_id", rows.iter().map(|r| r.1).collect());
        columns.i64("frame_index", rows.iter().map(|r| r.2 as i64).collect());
        columns.u64("pc", rows.iter().map(|r| r.3).collect());
        store::write_table(
            &self.directory,
            "allocation_stack_raw",
            &[columns.finish()?],
        )?;
        Ok(())
    }
}
fn operations_batch(records: &[(Allocation, u64)]) -> anyhow::Result<RecordBatch> {
    let mut columns = Columns::default();
    macro_rules! u {
        ($field:ident) => {
            columns.u64(
                stringify!($field),
                records.iter().map(|(r, _)| u64::from(r.$field)).collect(),
            );
        };
    }
    u!(pid);
    u!(process_epoch);
    u!(sequence);
    u!(timestamp_ns);
    u!(tid);
    u!(operation_id);
    u!(old_address);
    u!(address);
    u!(requested_bytes);
    columns.u64(
        "stack_status",
        records.iter().map(|(r, _)| r.attribution.code()).collect(),
    );
    columns.u64(
        "operation",
        records.iter().map(|(r, _)| r.operation.code()).collect(),
    );
    columns.u64(
        "disposition",
        records
            .iter()
            .map(|(r, _)| u64::from(r.retires_old))
            .collect(),
    );
    columns.u64(
        "stack_id",
        records.iter().map(|(_, stack)| *stack).collect(),
    );
    columns.finish()
}
