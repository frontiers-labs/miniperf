use perf_event_open_sys::bindings::{
    PERF_BR_CALL, PERF_BR_COND_CALL, PERF_BR_COND_RET, PERF_BR_IND_CALL, PERF_BR_RET,
    PERF_SAMPLE_BRANCH_ANY, PERF_SAMPLE_BRANCH_ANY_CALL, PERF_SAMPLE_BRANCH_CALL_STACK,
    PERF_SAMPLE_BRANCH_TYPE_SAVE, PERF_SAMPLE_BRANCH_USER,
};
use smallvec::SmallVec;

/// How a host's branch recorder is asked to yield user call stacks.
///
/// Only Intel LBR maintains the call stack in hardware. AMD BRS and LbrV2, and
/// Arm BRBE, record a plain branch history, so the stack is rebuilt from the
/// call (and, where the history has them, return) records instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchMode {
    /// Intel LBR call-stack mode: the hardware keeps the stack.
    CallStack,
    /// Call branches only (AMD LbrV2): the recorded call sites are the stack,
    /// minus the calls that have already returned.
    Calls,
    /// Every taken branch (AMD BRS): calls and returns are replayed to
    /// reconstruct the stack.
    All,
}

impl BranchMode {
    /// Modes to try, best first. `perf_event_open` is the authoritative probe:
    /// every recorder rejects the modes it does not implement.
    pub const LADDER: [BranchMode; 3] = [BranchMode::CallStack, BranchMode::Calls, BranchMode::All];

    /// The `branch_sample_type` this mode asks the kernel for.
    pub fn sample_type(self) -> u64 {
        let filter = match self {
            BranchMode::CallStack => PERF_SAMPLE_BRANCH_CALL_STACK,
            BranchMode::Calls => PERF_SAMPLE_BRANCH_ANY_CALL,
            // Replaying a branch history needs to tell calls from returns, and
            // only the kernel's branch-type decode is portable across vendors.
            BranchMode::All => PERF_SAMPLE_BRANCH_ANY | PERF_SAMPLE_BRANCH_TYPE_SAVE,
        };
        (filter | PERF_SAMPLE_BRANCH_USER) as u64
    }

    /// Rebuild a call stack, innermost frame first, from branch records
    /// ordered most recent first.
    pub fn frames(
        self,
        ip: u64,
        entries: impl Iterator<Item = BranchRecord>,
    ) -> SmallVec<[u64; 8]> {
        let mut frames: SmallVec<[u64; 8]> = SmallVec::new();
        frames.push(ip);
        match self {
            BranchMode::CallStack | BranchMode::Calls => {
                for entry in entries {
                    if entry.from != 0 && frames.last().copied() != Some(entry.from) {
                        frames.push(entry.from);
                    }
                }
            }
            BranchMode::All => {
                let history: SmallVec<[BranchRecord; 32]> = entries.collect();
                let mut stack: SmallVec<[u64; 8]> = SmallVec::new();
                for entry in history.iter().rev() {
                    match entry.kind() {
                        BranchKind::Call if entry.from != 0 => stack.push(entry.from),
                        BranchKind::Return => {
                            stack.pop();
                        }
                        _ => {}
                    }
                }
                frames.extend(stack.into_iter().rev());
            }
        }
        frames
    }
}

/// One hardware branch record: where the branch was taken from, plus the
/// packed flag word that carries its type.
#[derive(Clone, Copy)]
pub struct BranchRecord {
    pub from: u64,
    pub flags: u64,
}

enum BranchKind {
    Call,
    Return,
    Other,
}

impl BranchRecord {
    /// `perf_branch_entry` keeps the branch type in bits 20..24, after the
    /// four prediction flags and the 16-bit cycle count.
    fn kind(self) -> BranchKind {
        match ((self.flags >> 20) & 0xf) as u32 {
            PERF_BR_CALL | PERF_BR_IND_CALL | PERF_BR_COND_CALL => BranchKind::Call,
            PERF_BR_RET | PERF_BR_COND_RET => BranchKind::Return,
            _ => BranchKind::Other,
        }
    }
}
