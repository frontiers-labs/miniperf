#[path = "../../collector-core/src/control.rs"]
mod control;

use control::{CollectorStats, ControlClient, ControlCommand, ControlPlane};
use shmem::proc_channel::{Receiver, Sender};

fn main() {
    let pid = std::process::id();
    let prefix = format!("/mc{pid:x}");
    // This check changes its environment before starting any other threads.
    unsafe { std::env::set_var("MPERF_CONTROL_SHMEM", &prefix) };
    let mut plane = ControlPlane::create(pid).expect("create control plane");
    let commands = unsafe { Sender::attach(&format!("{prefix}-{pid}-cmd"), 4 * 1024) }
        .expect("attach sole command sender");
    let mut stats = unsafe { Receiver::attach(&format!("{prefix}-{pid}-stats"), 64 * 1024) }
        .expect("attach sole statistics receiver");

    for bytes in [&[][..], &[1], &[1, 0, 0], &[1, 0, 0, 0, 0], &[255, 0, 0, 0]] {
        commands.send_sync(bytes).unwrap();
    }
    commands.send_sync(&[2, 0, 0, 0]).unwrap();
    assert_eq!(plane.poll_command(), Some(ControlCommand::Resume));
    assert_eq!(plane.poll_command(), None);
    for (bytes, command) in [
        ([1, 0, 0, 0], ControlCommand::Pause),
        ([3, 0, 0, 0], ControlCommand::Flush),
    ] {
        commands.send_sync(&bytes).unwrap();
        assert_eq!(plane.poll_command(), Some(command));
    }
    plane.publish(CollectorStats {
        pid: 7,
        events: 8,
        dropped: 9,
        timestamp: 10,
    });
    let mut expected = 7_u32.to_le_bytes().to_vec();
    for value in [8_u64, 9, 10] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(stats.try_recv(), Some(expected));
    plane.close();
    assert_eq!(stats.recv_sync(), None);
    drop((commands, stats, plane));

    let prefix = format!("/md{pid:x}");
    let stats = Sender::new(&format!("{prefix}-{pid}-stats"), 64 * 1024).unwrap();
    let mut commands = Receiver::new(&format!("{prefix}-{pid}-cmd"), 4 * 1024).unwrap();
    let mut client =
        unsafe { ControlClient::attach(&prefix, pid) }.expect("attach sole control client");
    for command in [
        ControlCommand::Pause,
        ControlCommand::Resume,
        ControlCommand::Flush,
    ] {
        client.send(command);
        assert_eq!(
            commands.try_recv(),
            Some((command as u32).to_le_bytes().to_vec())
        );
    }
    for timestamp in [1_u64, 2] {
        let mut bytes = 3_u32.to_le_bytes().to_vec();
        for value in [4, 5, timestamp] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        stats.send_sync(&bytes).unwrap();
        stats.send_sync(&bytes[..27]).unwrap();
        bytes.push(0);
        stats.send_sync(&bytes).unwrap();
    }
    let latest = client.latest_stats().expect("latest valid stats");
    assert_eq!(
        (latest.pid, latest.events, latest.dropped, latest.timestamp),
        (3, 4, 5, 2)
    );
    assert!(client.latest_stats().is_none());
    drop((client, commands, stats));
    println!(
        "control protocol accepts explicit little-endian messages and skips malformed messages"
    );
}
