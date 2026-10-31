use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::datatypes::Schema;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

/// Rolling Parquet writer: closes the current file (with a proper footer) and
/// starts a new one once it grows past the segment limit, so a crash loses at
/// most the open segment.
pub struct SegmentWriter {
    dir: PathBuf,
    table: String,
    suffix: String,
    schema: Arc<Schema>,
    max_bytes: u64,
    seq: u32,
    current: Option<ArrowWriter<File>>,
    finished: Vec<PathBuf>,
}

const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

fn writer_properties(table: &str) -> WriterProperties {
    let mut properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(1).unwrap()));
    if table == "events" {
        properties = properties
            .set_column_dictionary_enabled("timestamp".into(), false)
            .set_column_dictionary_enabled("instance".into(), false);
    }
    properties.build()
}

impl SegmentWriter {
    /// A writer for `<table>-<pid>-<seq>.parquet` segments (or
    /// `<table>-<seq>.parquet` when the table is not per-process).
    pub fn new(dir: &Path, table: &str, pid: Option<u32>, schema: Arc<Schema>) -> Self {
        let suffix = match pid {
            Some(pid) => format!("-{pid}"),
            None => String::new(),
        };
        SegmentWriter {
            dir: dir.to_owned(),
            table: table.to_owned(),
            suffix,
            schema,
            max_bytes: DEFAULT_SEGMENT_BYTES,
            seq: 0,
            current: None,
            finished: Vec::new(),
        }
    }

    pub fn with_segment_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    fn segment_path(&self) -> PathBuf {
        self.dir.join(format!(
            "{}{}-{}.parquet",
            self.table, self.suffix, self.seq
        ))
    }

    pub fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        if self.current.is_none() {
            let path = self.segment_path();
            let file = File::create(&path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            self.current = Some(ArrowWriter::try_new(
                file,
                self.schema.clone(),
                Some(writer_properties(&self.table)),
            )?);
        }
        let writer = self.current.as_mut().unwrap();
        writer.write(batch)?;
        writer.flush()?;
        if writer.bytes_written() as u64 >= self.max_bytes {
            self.roll()?;
        }
        Ok(())
    }

    fn roll(&mut self) -> Result<()> {
        if let Some(writer) = self.current.take() {
            writer.close()?;
            self.finished.push(self.segment_path());
            self.seq += 1;
        }
        Ok(())
    }

    /// Close the open segment and return every file written so far. Writing
    /// again after `finish` starts a new segment.
    pub fn finish(&mut self) -> Result<Vec<PathBuf>> {
        self.roll()?;
        Ok(std::mem::take(&mut self.finished))
    }
}

/// Write one whole table as a single `<name>.parquet` file, replacing any
/// existing file. Used for postprocess-derived tables.
pub fn write_table(dir: &Path, name: &str, batches: &[RecordBatch]) -> Result<PathBuf> {
    let schema = batches
        .first()
        .map(|batch| batch.schema())
        .context("write_table requires at least one batch")?;
    let path = dir.join(format!("{name}.parquet"));
    let file =
        File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(writer_properties(name)))?;
    for batch in batches {
        writer.write(batch)?;
    }
    writer.close()?;
    Ok(path)
}
