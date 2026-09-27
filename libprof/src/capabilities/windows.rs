//! Windows host capability metadata. Active ETW readiness is probed by the
//! Windows driver, which reports permission and PMU failures separately.

use super::Capabilities;

pub(super) fn capabilities() -> Capabilities {
    Capabilities {
        arch: std::env::consts::ARCH.to_owned(),
        ..Capabilities::default()
    }
}
