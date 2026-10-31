mod buffer {
    include!(concat!(env!("MPERF_COLLECTOR_SOURCE"), "/buffer.rs"));
}
mod control {
    include!(concat!(env!("MPERF_COLLECTOR_SOURCE"), "/control.rs"));
}
mod stack {
    include!(concat!(env!("MPERF_COLLECTOR_SOURCE"), "/stack.rs"));
}
mod api {
    include!(concat!(env!("MPERF_COLLECTOR_SOURCE"), "/api.rs"));
}
mod session {
    include!(concat!(env!("MPERF_COLLECTOR_SOURCE"), "/session.rs"));

    // Access only the real publication lock; recording uses the production ABI.
    pub fn hold_lock(ready: std::sync::mpsc::Sender<()>, release: std::sync::mpsc::Receiver<()>) {
        let _guard = process_collector().collector.write().unwrap();
        ready.send(()).unwrap();
        release.recv().unwrap();
    }

    pub fn actual_tid() -> u32 {
        current_thread_id()
    }
}

use std::io::{Read, Write};
use std::path::Path;
use store::arrow::array::{Int64Array, UInt32Array};
use store::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

pub const SESSION_DIR_ENV: &str = "MPERF_SESSION_DIR";

fn register() -> *mut session::HandleData {
    let payload = api::MperfTracePayload {
        name: c"fork-event".as_ptr(),
        function: c"main".as_ptr(),
        file: c"collector_fork.rs".as_ptr(),
        line: 1,
        column: 1,
        flags: 0,
    };
    unsafe { api::mperf_trace_register(&payload) }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let directory = Path::new(&args[1]);
    let allocation = args[2] == "allocation";
    let shutdown_only = args[2] == "shutdown";
    let channel_name = format!("/mperf-fork-check-{}", std::process::id());
    let mut allocation_channel = allocation.then(|| {
        shmem::proc_channel::Receiver::new(&channel_name, shmem::allocation::CHANNEL_BYTES).unwrap()
    });
    // Environment is fixed before the collector starts its writer thread.
    unsafe {
        std::env::set_var(SESSION_DIR_ENV, directory);
        std::env::remove_var("MPERF_ALLOCATION_CHANNEL");
        if allocation {
            std::env::set_var("MPERF_ALLOCATION_CHANNEL", channel_name);
        }
    }
    let parent = std::process::id();
    let parent_tid = session::actual_tid();
    let handle = register();
    assert!(!handle.is_null());
    unsafe { api::mperf_trace_instant(handle, 1) };
    let (mut parent_socket, mut child_socket) = std::os::unix::net::UnixStream::pair().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || session::hold_lock(ready_tx, release_rx));
    ready_rx.recv().unwrap();
    eprintln!(
        "{mode}: forking with the production collector lock held",
        mode = args[2]
    );
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        unsafe { libc::alarm(5) };
        child_socket
            .write_all(&session::actual_tid().to_ne_bytes())
            .unwrap();
        if allocation {
            assert!(register().is_null(), "allocation children remain excluded");
        } else if !shutdown_only {
            // A different child thread may initialize before the surviving one
            // accesses its inherited TLS cache.
            std::thread::spawn(|| assert!(!register().is_null()))
                .join()
                .unwrap();
            // This first event must abandon both the inherited cache and slot.
            unsafe { api::mperf_trace_instant(handle, 3) };
            assert!(!register().is_null());
            unsafe { api::mperf_trace_instant(handle, 4) };
        }
        if args[2] != "trace-atexit" {
            api::mperf_trace_shutdown();
            unsafe { api::mperf_trace_instant(handle, 99) };
        }
        unsafe { libc::exit(0) };
    }
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert_eq!(
        status, 0,
        "fork child failed or timed out, wait status {status}"
    );
    let mut child_tid = [0_u8; 4];
    parent_socket.read_exact(&mut child_tid).unwrap();
    let child_tid = u32::from_ne_bytes(child_tid);
    unsafe { api::mperf_trace_instant(handle, 2) };
    api::mperf_trace_shutdown();
    unsafe { api::mperf_trace_instant(handle, 99) };

    let mut rows = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if !path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("events-")
        {
            continue;
        }
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).unwrap())
            .unwrap()
            .build()
            .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let pids = batch
                .column_by_name("pid")
                .unwrap()
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            let tids = batch
                .column_by_name("tid")
                .unwrap()
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            let values = batch
                .column_by_name("value")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            rows.extend(
                (0..batch.num_rows()).map(|i| (pids.value(i), tids.value(i), values.value(i))),
            );
        }
    }
    rows.sort_unstable();
    let mut expected = vec![(parent, parent_tid, 1), (parent, parent_tid, 2)];
    if !allocation && !shutdown_only {
        expected.extend([(child as u32, child_tid, 3), (child as u32, child_tid, 4)]);
    }
    expected.sort_unstable();
    assert_eq!(
        rows, expected,
        "real event segments must contain every expected PID, TID and value"
    );
    if let Some(receiver) = &mut allocation_channel {
        let footer = receiver.recv_sync().expect("root allocation footer");
        let footer = shmem::allocation::Batch::decode(&footer).unwrap();
        assert!(footer.footer && footer.records.is_empty());
    }
    println!(
        "{}: lock-held fork completed; exact parent/child records and shutdown verified",
        args[2]
    );
}
