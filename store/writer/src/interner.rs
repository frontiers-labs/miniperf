use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use crate::hash::string_hash;
use crate::tables::StringRows;
use crate::writer::SegmentWriter;

/// Deduplicating string dictionary. IDs are XXH3-64 of the string, so the same
/// string gets the same ID in every process, rank, and pass.
pub struct StringInterner {
    seen: HashSet<u64>,
    rows: StringRows,
    writer: SegmentWriter,
}

impl StringInterner {
    pub fn new(dir: &Path, pid: Option<u32>) -> Self {
        StringInterner {
            seen: HashSet::new(),
            rows: StringRows::default(),
            writer: SegmentWriter::new(dir, "strings", pid, StringRows::schema()),
        }
    }

    /// Intern a string and return its stable ID.
    pub fn intern(&mut self, value: &str) -> u64 {
        let id = string_hash(value);
        if self.seen.insert(id) {
            self.rows.id.push(id);
            self.rows.string.push(value.to_owned());
        }
        id
    }

    pub fn flush(&mut self) -> Result<()> {
        if !self.rows.is_empty() {
            let batch = self.rows.to_batch()?;
            self.writer.write(&batch)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        self.flush()?;
        self.writer.finish()?;
        Ok(())
    }
}
