//! Versioned allocation lifecycle protocol shared by the collector and source.

pub const CHANNEL_BYTES: usize = 1 << 24;
pub const MAX_FRAMES: usize = 32;
pub const RECORD_BYTES: usize = 104 + MAX_FRAMES * 8;
const MAGIC: &[u8; 8] = b"MPALLOC1";

/// Application-visible allocator ownership boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Operation {
    Allocated = 1,
    Freed = 2,
    ReallocBegin = 3,
    ReallocEnd = 4,
}

impl TryFrom<u64> for Operation {
    type Error = std::io::Error;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Allocated),
            2 => Ok(Self::Freed),
            3 => Ok(Self::ReallocBegin),
            4 => Ok(Self::ReallocEnd),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown allocation operation",
            )),
        }
    }
}

/// A dynamic operation, identified independently of its address or call site.
#[derive(Debug, Clone)]
pub struct Allocation {
    pub pid: u64,
    pub process_epoch: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub tid: u64,
    pub operation: Operation,
    pub operation_id: u64,
    pub old_address: u64,
    pub address: u64,
    pub requested_bytes: u64,
    /// Successful replacement, or a platform-confirmed zero-size retirement.
    pub retires_old: bool,
    /// 0 not selected, 1 captured, 2 unwind unavailable, 3 depth bound reached, 4 unwind stopped with an error.
    pub stack_status: u64,
    pub frame_count: usize,
    pub frames: [u64; MAX_FRAMES],
}

impl Allocation {
    pub fn encode(&self, bytes: &mut Vec<u8>) {
        let header = [
            self.pid,
            self.process_epoch,
            self.sequence,
            self.timestamp_ns,
            self.tid,
            self.operation as u64,
            self.operation_id,
            self.old_address,
            self.address,
            self.requested_bytes,
            u64::from(self.retires_old),
            self.frame_count as u64,
            self.stack_status,
        ]
        .map(u64::to_le_bytes);
        let frames = self.frames.map(u64::to_le_bytes);
        bytes.reserve(RECORD_BYTES);
        bytes.extend_from_slice(header.as_flattened());
        bytes.extend_from_slice(frames.as_flattened());
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != RECORD_BYTES {
            return None;
        }
        let word = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        if word(0) > u32::MAX as u64 || word(4) > u32::MAX as u64 {
            return None;
        }
        let operation = Operation::try_from(word(5)).ok()?;
        let frame_count = usize::try_from(word(11)).ok()?;
        if frame_count > MAX_FRAMES || word(10) > 1 || word(12) > 4 {
            return None;
        }
        let mut frames = [0; MAX_FRAMES];
        for (i, frame) in frames.iter_mut().enumerate() {
            *frame = word(13 + i);
        }
        Some(Self {
            pid: word(0),
            process_epoch: word(1),
            sequence: word(2),
            timestamp_ns: word(3),
            tid: word(4),
            operation,
            operation_id: word(6),
            old_address: word(7),
            address: word(8),
            requested_bytes: word(9),
            retires_old: word(10) == 1,
            stack_status: word(12),
            frame_count,
            frames,
        })
    }
}

/// Writer batch; the footer reports independent producer and transport losses.
#[derive(Debug)]
pub struct Batch {
    pub sequence: u64,
    pub produced: u64,
    pub producer_dropped: u64,
    pub channel_dropped_operations: u64,
    pub paused: u64,
    pub footer: bool,
    pub records: Vec<Allocation>,
}

impl Batch {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(64 + self.records.len() * RECORD_BYTES);
        bytes.extend_from_slice(MAGIC);
        for word in [
            self.sequence,
            self.produced,
            self.producer_dropped,
            self.channel_dropped_operations,
            self.paused,
            u64::from(self.footer),
            self.records.len() as u64,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for record in &self.records {
            record.encode(&mut bytes);
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 64 || &bytes[..8] != MAGIC {
            return None;
        }
        let word = |i: usize| u64::from_le_bytes(bytes[8 + i * 8..16 + i * 8].try_into().unwrap());
        let count = usize::try_from(word(6)).ok()?;
        if word(5) > 1 || bytes.len() != 64usize.checked_add(count.checked_mul(RECORD_BYTES)?)? {
            return None;
        }
        let records = bytes[64..]
            .as_chunks::<RECORD_BYTES>()
            .0
            .iter()
            .map(|bytes| Allocation::decode(bytes))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            sequence: word(0),
            produced: word(1),
            producer_dropped: word(2),
            channel_dropped_operations: word(3),
            paused: word(4),
            footer: word(5) == 1,
            records,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Allocation, Batch, Operation, MAX_FRAMES};

    fn batch() -> Batch {
        Batch {
            sequence: 1,
            produced: 2,
            producer_dropped: 3,
            channel_dropped_operations: 4,
            paused: 5,
            footer: true,
            records: vec![Allocation {
                pid: 6,
                process_epoch: 7,
                sequence: 8,
                timestamp_ns: 9,
                tid: 10,
                operation: Operation::ReallocEnd,
                operation_id: 12,
                old_address: 13,
                address: 14,
                requested_bytes: 15,
                retires_old: true,
                stack_status: 3,
                frame_count: 2,
                frames: [16; MAX_FRAMES],
            }],
        }
    }

    #[test]
    fn batch_bytes_preserve_allocation_protocol() {
        let bytes = batch().encode();
        let mut expected = b"MPALLOC1".to_vec();
        for word in [
            1_u64, 2, 3, 4, 5, 1, 1, 6, 7, 8, 9, 10, 4, 12, 13, 14, 15, 1, 2, 3,
        ] {
            expected.extend_from_slice(&word.to_le_bytes());
        }
        for _ in 0..MAX_FRAMES {
            expected.extend_from_slice(&16_u64.to_le_bytes());
        }
        assert_eq!(bytes, expected);
        let decoded = Batch::decode(&bytes).unwrap();
        assert_eq!(decoded.encode(), expected);
    }

    #[test]
    fn batch_rejects_truncation_and_extra_bytes() {
        let mut bytes = batch().encode();
        for length in 0..bytes.len() {
            assert!(Batch::decode(&bytes[..length]).is_none(), "length {length}");
        }
        bytes.push(0);
        assert!(Batch::decode(&bytes).is_none());
    }

    #[test]
    fn batch_rejects_invalid_header_fields() {
        for (offset, value) in [(0, 0_u64), (48, 2), (56, u64::MAX)] {
            let mut bytes = batch().encode();
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            assert!(Batch::decode(&bytes).is_none(), "offset {offset}");
        }
    }

    #[test]
    fn batch_rejects_invalid_allocation_fields() {
        for (word, value) in [
            (0, u64::MAX),
            (4, u64::MAX),
            (5, 0),
            (10, 2),
            (11, 33),
            (12, 5),
        ] {
            let mut bytes = batch().encode();
            let offset = 64 + word * 8;
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            assert!(Batch::decode(&bytes).is_none(), "record word {word}");
        }
    }

    #[test]
    fn empty_footer_decodes_without_records() {
        let mut footer = batch();
        footer.records.clear();
        let bytes = footer.encode();
        assert_eq!(bytes.len(), 64);
        let decoded = Batch::decode(&bytes).unwrap();
        assert!(decoded.footer);
        assert!(decoded.records.is_empty());
    }
}
