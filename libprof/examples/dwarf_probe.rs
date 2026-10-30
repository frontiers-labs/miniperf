//! Checks post-hoc DWARF unwinding against a live Rust caller chain.
//!
//! ```sh
//! cargo rustc -p libprof --example dwarf_probe --features symbolize -- \
//!   -C force-frame-pointers=no -C force-unwind-tables=yes -C link-arg=-fuse-ld=lld
//! ./target/debug/examples/dwarf_probe
//! ```
//! Add `-C relocation-model=static -C link-arg=-no-pie` to check non-PIE ELF.

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "symbolize"))]
mod probe {
    use std::{arch::asm, fs::File, hint::black_box, os::unix::fs::FileExt};

    use anyhow::{bail, Context};
    use libprof::{process_modules, PostHocUnwinder, StackSample};

    struct Capture {
        regs: [u64; 3],
        stack: Vec<u8>,
    }

    #[inline(never)]
    fn capture(mem: &File, stack_end: u64) -> anyhow::Result<Capture> {
        let (rip, rsp, rbp): (u64, u64, u64);
        // Keep the PC and stack registers at the same instruction boundary.
        unsafe {
            asm!(
                "lea {rip}, [rip + 2f]",
                "mov {rsp}, rsp",
                "mov {rbp}, rbp",
                "2:",
                rip = out(reg) rip,
                rsp = out(reg) rsp,
                rbp = out(reg) rbp,
                options(nostack, preserves_flags),
            );
        }
        let len = stack_end
            .checked_sub(rsp)
            .context("sampled SP is outside the stack mapping")?;
        let mut stack = vec![0; usize::try_from(len)?];
        mem.read_exact_at(&mut stack, rsp)?;
        Ok(Capture {
            // PERF_REG_X86_BP, SP, IP are indices 6, 7, 8.
            regs: [rbp, rsp, rip],
            stack,
        })
    }

    #[inline(never)]
    fn caller_middle(mem: &File, stack_end: u64) -> anyhow::Result<Capture> {
        let captured = capture(mem, stack_end);
        black_box(&captured);
        captured
    }

    #[inline(never)]
    fn caller_outer(mem: &File, stack_end: u64) -> anyhow::Result<Capture> {
        let captured = caller_middle(mem, stack_end);
        black_box(&captured);
        captured
    }

    pub fn run() -> anyhow::Result<()> {
        let pid = std::process::id();
        let modules = process_modules(pid);
        let marker = 0_u8;
        let stack_address = (&marker as *const u8) as u64;
        let stack_end = std::fs::read_to_string("/proc/self/maps")?
            .lines()
            .filter_map(|line| line.split_whitespace().next()?.split_once('-'))
            .filter_map(|(start, end)| {
                Some((
                    u64::from_str_radix(start, 16).ok()?,
                    u64::from_str_radix(end, 16).ok()?,
                ))
            })
            .find(|&(start, end)| start <= stack_address && stack_address < end)
            .map(|(_, end)| end)
            .context("could not find the live stack in /proc/self/maps")?;
        let mem = File::open("/proc/self/mem")?;
        let captured = caller_outer(&mem, stack_end)?;
        let sample = StackSample {
            pid,
            group_id: 1,
            regs_mask: (1 << 6) | (1 << 7) | (1 << 8),
            regs: &captured.regs,
            user_stack: &captured.stack,
            callchain: &[],
        };
        let mut unwinder = PostHocUnwinder::new(&modules);
        let frames = unwinder.resolve(&sample);
        let resolver = symbolize::Resolver::new(modules.iter().map(|map| symbolize::ProcessMap {
            pid: map.pid,
            path: map.filename.clone().into(),
            start: map.addr,
            end: map.addr + map.len,
            offset: map.pgoff,
        }));
        println!("captured {} stack bytes", captured.stack.len());
        let mut names = Vec::new();
        for (index, &ip) in frames.iter().enumerate() {
            // Return addresses point just beyond the calling instruction.
            let address = if index == 0 { ip } else { ip - 1 };
            let resolved = resolver.resolve(pid, address);
            let name = resolved
                .first()
                .map_or("<unknown>", |frame| frame.function.as_str());
            println!("{index:2} {ip:#018x} {name}");
            names.push(name.to_owned());
        }
        let expected = [
            "dwarf_probe::probe::capture",
            "dwarf_probe::probe::caller_middle",
            "dwarf_probe::probe::caller_outer",
        ];
        let mut remaining = names.iter();
        for caller in expected {
            if !remaining.any(|name| name == caller) {
                bail!("DWARF did not recover {caller} in caller order");
            }
        }
        println!("recovered the live Rust caller chain");
        Ok(())
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "symbolize"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", feature = "symbolize")))]
fn main() {
    eprintln!("dwarf_probe requires Linux x86_64 and --features symbolize");
    std::process::exit(3);
}
