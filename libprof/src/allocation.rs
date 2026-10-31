//! Allocation ownership records, independent of capture transport.

/// Application-visible ownership operation.
#[derive(Debug, Clone, Copy)]
pub enum AllocationOperation {
    /// Successful introduction after the allocator returns.
    Allocated,
    /// Retirement before free enters the allocator.
    Freed,
    /// Pre-call realloc boundary, paired by its sequence.
    ReallocBegin,
    /// Post-call realloc outcome, paired by operation_id.
    ReallocEnd,
}
impl AllocationOperation {
    /// Stable recording representation.
    pub fn code(self) -> u64 {
        match self {
            Self::Allocated => 1,
            Self::Freed => 2,
            Self::ReallocBegin => 3,
            Self::ReallocEnd => 4,
        }
    }
}

/// Attribution quality. Lifetimes are collected independently of this policy.
#[derive(Debug)]
pub enum AllocationAttribution {
    /// No stack was requested for this operation.
    NotSelected,
    /// Raw PCs captured using the available unwind information.
    Captured(Vec<u64>),
    /// Stack capture returned no frames.
    Unavailable,
    /// Raw PCs reached the configured depth bound.
    Truncated(Vec<u64>),
    /// The unwinder stopped with an error; any retained PCs are partial.
    UnwindError(Vec<u64>),
}
impl AllocationAttribution {
    /// Captured PCs, ordered from the allocator to its callers.
    pub fn frames(&self) -> &[u64] {
        match self {
            Self::Captured(frames) | Self::Truncated(frames) | Self::UnwindError(frames) => frames,
            Self::NotSelected | Self::Unavailable => &[],
        }
    }
    /// Stable recording representation of attribution quality.
    pub fn code(&self) -> u64 {
        match self {
            Self::NotSelected => 0,
            Self::Captured(_) => 1,
            Self::Unavailable => 2,
            Self::Truncated(_) => 3,
            Self::UnwindError(_) => 4,
        }
    }
}

/// One observed allocation ownership boundary. A generation's identity is the
/// allocating process epoch and introduction sequence, never its address.
#[derive(Debug)]
pub struct Allocation {
    /// Root process identifier.
    pub pid: u32,
    /// Process capture epoch, disambiguating repeated PIDs.
    pub process_epoch: u64,
    /// Process-wide operation order.
    pub sequence: u64,
    /// CLOCK_MONOTONIC ownership boundary.
    pub timestamp_ns: u64,
    /// Thread that performed the operation.
    pub tid: u32,
    /// Ownership operation.
    pub operation: AllocationOperation,
    /// Realloc begin sequence, zero for ordinary allocation/free.
    pub operation_id: u64,
    /// Old pointer for free or realloc.
    pub old_address: u64,
    /// Successful result pointer; zero for failed or retirement operations.
    pub address: u64,
    /// Application-requested bytes.
    pub requested_bytes: u64,
    /// Realloc outcome confirms retirement of the old generation.
    pub retires_old: bool,
    /// Raw stack attribution and its quality.
    pub attribution: AllocationAttribution,
}
