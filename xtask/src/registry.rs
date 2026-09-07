//! The single declaration of every check and every runner.
//!
//! CI reads this through `cargo xtask matrix`, so no workflow file names a
//! check or a runner label and the two cannot drift apart.

/// What a check reads. This alone decides which pipeline runs it: `Source` in
/// the lint job, `Package` in the test stage, `Recordings` in the GUI pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Needs {
    Source,
    Package,
    Recordings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }
}

/// How far a runner's hardware counters can be trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pmu {
    /// No PMU. `needs_pmu` checks are not scheduled here at all.
    Absent,
    /// A PMU on most placements. Measured at 9 of 10 on Blacksmith x86, where
    /// AMD hosts expose one and Intel hosts do not.
    Flaky,
    /// A PMU on every placement. Absence here means the fleet changed.
    Reliable,
}

pub struct Runner {
    pub label: &'static str,
    /// Package platform name, as produced by `utils/package-miniperf.sh`.
    pub platform: &'static str,
    pub os: Os,
    pub pmu: Pmu,
    /// Reruns allowed when the PMU oracle reports no counters.
    pub reruns: u8,
}

pub struct Check {
    pub name: &'static str,
    pub needs: Needs,
    pub os: &'static [Os],
    pub needs_pmu: bool,
    pub apt: &'static [&'static str],
    /// Uploads its output directory as `recordings-<platform>`.
    pub records: bool,
}

const LINUX: &[Os] = &[Os::Linux];
const DESKTOP: &[Os] = &[Os::Linux, Os::Macos, Os::Windows];
const UNIX: &[Os] = &[Os::Linux, Os::Macos];

pub const RUNNERS: &[Runner] = &[
    Runner {
        label: "ubuntu-24.04",
        platform: "linux-x86_64",
        os: Os::Linux,
        pmu: Pmu::Absent,
        reruns: 0,
    },
    Runner {
        label: "ubuntu-24.04-arm",
        platform: "linux-aarch64",
        os: Os::Linux,
        pmu: Pmu::Reliable,
        reruns: 0,
    },
    Runner {
        label: "blacksmith-2vcpu-ubuntu-2404",
        platform: "linux-x86_64",
        os: Os::Linux,
        pmu: Pmu::Flaky,
        reruns: 1,
    },
    Runner {
        label: "macos-14",
        platform: "macos-aarch64",
        os: Os::Macos,
        pmu: Pmu::Absent,
        reruns: 0,
    },
    Runner {
        label: "windows-2022",
        platform: "windows-x86_64",
        os: Os::Windows,
        pmu: Pmu::Absent,
        reruns: 0,
    },
];

/// Targets the build stage packages. Kept separate from `RUNNERS` because
/// riscv64 is cross-compiled from an x86 runner and never runs a check.
pub const PACKAGE_TARGETS: &[(&str, &str, &str)] = &[
    ("linux-x86_64", "x86_64-unknown-linux-gnu", "ubuntu-24.04"),
    (
        "linux-aarch64",
        "aarch64-unknown-linux-gnu",
        "ubuntu-24.04-arm",
    ),
    (
        "linux-riscv64",
        "riscv64gc-unknown-linux-gnu",
        "ubuntu-24.04",
    ),
    ("macos-aarch64", "aarch64-apple-darwin", "macos-14"),
    ("windows-x86_64", "x86_64-pc-windows-msvc", "windows-2022"),
];

pub const CHECKS: &[Check] = &[
    // Lint: reads the source tree only.
    lint("fmt"),
    lint("clippy"),
    lint("shellcheck"),
    lint("cfg-guard"),
    lint("deps-manifest"),
    lint("test-policy"),
    lint("no-fixtures"),
    lint("registry"),
    // Test: reads an unpacked package.
    Check {
        name: "package-layout",
        needs: Needs::Package,
        os: DESKTOP,
        needs_pmu: false,
        apt: &[],
        records: false,
    },
    Check {
        name: "cli",
        needs: Needs::Package,
        os: UNIX,
        needs_pmu: false,
        apt: &[],
        records: false,
    },
    Check {
        name: "doctor",
        needs: Needs::Package,
        os: UNIX,
        needs_pmu: false,
        apt: &[],
        records: false,
    },
    Check {
        name: "pmu-snapshot",
        needs: Needs::Package,
        os: LINUX,
        needs_pmu: true,
        apt: &[],
        records: true,
    },
    Check {
        name: "pmu-tma",
        needs: Needs::Package,
        os: LINUX,
        needs_pmu: true,
        apt: &[],
        records: true,
    },
    Check {
        name: "pmu-duty-split",
        needs: Needs::Package,
        os: LINUX,
        needs_pmu: true,
        apt: &[],
        records: false,
    },
    Check {
        name: "query",
        needs: Needs::Package,
        os: LINUX,
        needs_pmu: true,
        apt: &[],
        records: false,
    },
    Check {
        name: "shim-libc",
        needs: Needs::Package,
        os: LINUX,
        needs_pmu: true,
        apt: &[],
        records: false,
    },
    // GUI: reads recordings produced by the test stage.
    Check {
        name: "gui-tour",
        needs: Needs::Recordings,
        os: DESKTOP,
        needs_pmu: false,
        apt: &["xvfb", "mesa-vulkan-drivers"],
        records: false,
    },
];

const fn lint(name: &'static str) -> Check {
    Check {
        name,
        needs: Needs::Source,
        os: LINUX,
        needs_pmu: false,
        apt: &[],
        records: false,
    }
}

pub fn checks_for(needs: Needs, os: Os, pmu: Pmu) -> Vec<&'static Check> {
    CHECKS
        .iter()
        .filter(|check| check.needs == needs)
        .filter(|check| check.os.contains(&os))
        .filter(|check| !check.needs_pmu || pmu != Pmu::Absent)
        .collect()
}
