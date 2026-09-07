//! macOS implementations of the host facilities in [`super`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{ProcessContext, ProcessIo, ProcessStat};
use crate::sink::ProcAddr;

const VM_PROT_EXECUTE: i32 = 0x4;

/// macOS exposes no `/proc`; a recording measures the root process alone.
pub(super) fn process_tree(_root_pid: u32) -> Option<Vec<ProcessStat>> {
    None
}

pub(super) fn process_tree_supported() -> bool {
    false
}

pub(super) fn process_io(_pid: u32) -> ProcessIo {
    ProcessIo::default()
}

pub(super) fn process_context(_pid: u32) -> ProcessContext {
    ProcessContext::default()
}

pub(super) fn process_modules(pid: u32) -> Vec<ProcAddr> {
    let dyld = proc_maps::mac_maps::get_dyld_info(pid as proc_maps::Pid)
        .ok()
        .filter(|images| !images.is_empty());
    if let Some(images) = dyld {
        let mut link_bases = HashMap::<PathBuf, Option<u64>>::new();
        let mut modules = Vec::new();
        for image in images {
            // proc-maps exposes every LC_SEGMENT_64 command, including
            // __PAGEZERO. It is not mapped and its multi-gigabyte virtual span
            // would falsely claim most user addresses during symbol lookup.
            if !segment_is_executable(image.segment.vmsize, image.segment.initprot) {
                continue;
            }
            let link_base = *link_bases
                .entry(image.filename.clone())
                .or_insert_with(|| text_address(&image.filename));
            let link_address = link_base
                .and_then(|base| {
                    let slide = (image.address as u64).checked_sub(base)?;
                    image.segment.vmaddr.checked_sub(slide)
                })
                .unwrap_or(image.segment.fileoff);
            modules.push(ProcAddr {
                pid,
                addr: image.segment.vmaddr,
                len: image.segment.vmsize,
                // For Mach-O, addr2line consumes link-time virtual addresses.
                // Store the unslid segment VM address here so
                // `runtime - address + offset` reconstructs that address.
                pgoff: link_address,
                filename: image.filename.to_string_lossy().into_owned(),
            });
        }
        return modules;
    }

    let Ok(maps) = proc_maps::get_process_maps(pid as proc_maps::Pid) else {
        return Vec::new();
    };
    maps.into_iter()
        .filter(|map| map.is_exec())
        .filter_map(|map| {
            Some(ProcAddr {
                pid,
                addr: map.start() as u64,
                len: map.size() as u64,
                pgoff: 0,
                filename: map.filename()?.to_string_lossy().into_owned(),
            })
        })
        .collect()
}

/// The link-time virtual address of a Mach-O binary's `__TEXT` segment.
fn text_address(path: &Path) -> Option<u64> {
    use object::{Object, ObjectSegment};

    let data = std::fs::read(path).ok()?;
    let object = object::File::parse(data.as_slice()).ok()?;
    object
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some("__TEXT"))
        .map(|segment| segment.address())
}

fn segment_is_executable(size: u64, initial_protection: i32) -> bool {
    size > 0 && initial_protection & VM_PROT_EXECUTE != 0
}

pub(super) fn current_thread_id() -> u64 {
    let mut tid = 0_u64;
    unsafe {
        libc::pthread_threadid_np(0, &mut tid);
    }
    tid
}
