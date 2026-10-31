use libc::EAGAIN;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{platform, utils::blocker};

/// The sole producer of byte messages in a shared-memory channel.
///
/// Messages must be encoded explicitly as bytes.
/// ```compile_fail
/// fn send_bool(sender: &shmem::proc_channel::Sender) {
///     sender.send_sync(&true).unwrap();
/// }
/// ```
pub struct Sender {
    inner: Inner,
}

/// The sole consumer of byte messages in a shared-memory channel.
///
/// Receiving requires an exclusive borrow.
/// ```compile_fail
/// fn receive_twice(receiver: &mut shmem::proc_channel::Receiver) {
///     let first = receiver.recv();
///     let second = receiver.recv();
///     drop((first, second));
/// }
/// ```
/// A receiver cannot be shared between threads.
/// ```compile_fail
/// fn require_sync<T: Sync>() {}
/// require_sync::<shmem::proc_channel::Receiver>();
/// ```
pub struct Receiver {
    inner: Inner,
}

/// A snapshot of the channel's backpressure counter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DropEvent {
    /// Total messages dropped since the channel was created.
    pub total: usize,
}

struct Inner {
    sem: platform::Semaphore,
    finish_sem: platform::Semaphore,
    shmem: platform::Shmem,
    ptr: *mut (),
    head: *mut usize,
    tail: *mut usize,
    dropped: *mut usize,
    size: usize,
}

impl Inner {
    fn semaphore_name(channel_name: &str, suffix: char) -> String {
        let mut hasher = DefaultHasher::new();
        channel_name.hash(&mut hasher);
        format!("/mp_{:016x}_{suffix}", hasher.finish())
    }

    fn data_offset() -> usize {
        assert_eq!(
            platform::Semaphore::required_size() % std::mem::align_of::<usize>(),
            0
        );
        let metadata_size =
            2 * platform::Semaphore::required_size() + 3 * std::mem::size_of::<usize>();
        metadata_size.next_multiple_of(std::mem::align_of::<usize>())
    }

    fn compute_size(data_size: usize) -> Result<usize, Error> {
        if !data_size.is_power_of_two() || data_size < 2 * std::mem::size_of::<usize>() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "shared-memory ring capacity must be a power of two and hold one record",
            ));
        }
        Self::data_offset().checked_add(data_size).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "shared-memory ring capacity overflows",
            )
        })
    }

    fn new(shmem: platform::Shmem, data_size: usize, init: bool) -> Result<Self, Error> {
        let sem_name = Self::semaphore_name(shmem.name(), 'r');
        let finish_sem_name = Self::semaphore_name(shmem.name(), 'f');
        let (sem, finish_sem) = unsafe {
            if init {
                (
                    platform::Semaphore::create(shmem.as_mut_ptr(), &sem_name)?,
                    platform::Semaphore::create(
                        shmem
                            .as_mut_ptr()
                            .byte_add(platform::Semaphore::required_size()),
                        &finish_sem_name,
                    )?,
                )
            } else {
                (
                    platform::Semaphore::open(shmem.as_mut_ptr(), &sem_name)?,
                    platform::Semaphore::open(
                        shmem
                            .as_mut_ptr()
                            .byte_add(platform::Semaphore::required_size()),
                        &finish_sem_name,
                    )?,
                )
            }
        };

        let sem_size = 2 * platform::Semaphore::required_size();
        let ptr = unsafe { shmem.as_mut_ptr().byte_add(Self::data_offset()) };
        let head = unsafe { shmem.as_mut_ptr().byte_add(sem_size).cast::<usize>() };
        let tail = unsafe {
            shmem
                .as_mut_ptr()
                .byte_add(sem_size + std::mem::size_of::<usize>())
                .cast::<usize>()
        };
        let dropped = unsafe {
            shmem
                .as_mut_ptr()
                .byte_add(sem_size + 2 * std::mem::size_of::<usize>())
                .cast::<usize>()
        };

        // Attaching must not reset live channel state.
        if init {
            unsafe {
                AtomicUsize::from_ptr(head).store(0, Ordering::Relaxed);
                AtomicUsize::from_ptr(tail).store(0, Ordering::Relaxed);
                AtomicUsize::from_ptr(dropped).store(0, Ordering::Relaxed);
            }
        }

        Ok(Self {
            shmem,
            sem,
            finish_sem,
            ptr,
            head,
            tail,
            dropped,
            size: data_size,
        })
    }

    unsafe fn write_wrapped(&self, position: usize, bytes: &[u8]) {
        let offset = position & (self.size - 1);
        let first = bytes.len().min(self.size - offset);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.byte_add(offset).cast(), first);
            if first < bytes.len() {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(first),
                    self.ptr.cast(),
                    bytes.len() - first,
                );
            }
        }
    }

    unsafe fn read_wrapped(&self, position: usize, bytes: &mut [u8]) {
        let offset = position & (self.size - 1);
        let first = bytes.len().min(self.size - offset);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.ptr.byte_add(offset).cast(),
                bytes.as_mut_ptr(),
                first,
            );
            if first < bytes.len() {
                std::ptr::copy_nonoverlapping(
                    self.ptr.cast(),
                    bytes.as_mut_ptr().add(first),
                    bytes.len() - first,
                );
            }
        }
    }

    fn head(&self) -> &AtomicUsize {
        unsafe { AtomicUsize::from_ptr(self.head) }
    }

    fn tail(&self) -> &AtomicUsize {
        unsafe { AtomicUsize::from_ptr(self.tail) }
    }

    fn dropped(&self) -> &AtomicUsize {
        unsafe { AtomicUsize::from_ptr(self.dropped) }
    }
}

impl Sender {
    /// Create a new channel and its sole sender, rejecting an existing name.
    pub fn new(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::create(name, Inner::compute_size(data_size)?)?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, true)?,
        })
    }

    /// Attach the sole endpoint of this kind to an initialized channel.
    ///
    /// # Safety
    /// The mapping must contain an initialized channel. The caller must ensure
    /// this is its only endpoint of this kind and that no process alters the
    /// mapping or its metadata outside the channel protocol.
    pub unsafe fn attach(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::open(name, Inner::compute_size(data_size)?)?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, false)?,
        })
    }

    pub fn name(&self) -> &str {
        self.inner.shmem.name()
    }

    /// Copies a message into the ring, or drops it and increments the counter if full.
    /// The copy and publication finish before return; the channel retains no borrow.
    pub fn send_sync(&self, data: &[u8]) -> Result<(), Error> {
        let word = std::mem::size_of::<usize>();
        let padded_len = data
            .len()
            .checked_add(word - 1)
            .map(|len| len & !(word - 1));
        let record_len = padded_len.and_then(|len| len.checked_add(word));

        let Some(record_len) = record_len else {
            self.inner.dropped().fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let tail = self.inner.tail().load(Ordering::Relaxed);
        let head = self.inner.head().load(Ordering::Acquire);
        if record_len > self.inner.size || tail.wrapping_sub(head) > self.inner.size - record_len {
            self.inner.dropped().fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        unsafe {
            self.inner.write_wrapped(tail, &data.len().to_ne_bytes());
            self.inner.write_wrapped(tail.wrapping_add(word), data);
        }
        // Publish only after the entire record is in shared memory.
        self.inner
            .tail()
            .store(tail.wrapping_add(record_len), Ordering::Release);
        self.inner.sem.post()
    }

    pub fn close(&self) -> Result<(), Error> {
        self.inner.finish_sem.post()?;
        // Wake a receiver which is blocked with an empty queue.
        self.inner.sem.post()
    }

    pub fn dropped_count(&self) -> usize {
        self.inner.dropped().load(Ordering::Relaxed)
    }

    pub fn drop_event(&self) -> DropEvent {
        DropEvent {
            total: self.dropped_count(),
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Receiver {
    /// Attach the sole endpoint of this kind to an initialized channel.
    ///
    /// # Safety
    /// The mapping must contain an initialized channel. The caller must ensure
    /// this is its only endpoint of this kind and that no process alters the
    /// mapping or its metadata outside the channel protocol.
    pub unsafe fn attach(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::open(name, Inner::compute_size(data_size)?)?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, false)?,
        })
    }

    /// Create a new channel and its sole receiver, rejecting an existing name.
    pub fn new(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::create(name, Inner::compute_size(data_size)?)?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, true)?,
        })
    }

    /// Read a queued packet without waiting. The receiver remains the sole consumer.
    pub fn try_recv(&mut self) -> Option<Vec<u8>> {
        if self.empty() {
            return None;
        }
        let _ = self.inner.sem.try_wait();
        self.read_one()
    }

    /// Wait for the next message or the sender to close the channel.
    pub fn recv_sync(&mut self) -> Option<Vec<u8>> {
        self.inner.sem.wait().ok()?;
        self.read_one()
    }

    /// Receive the next owned message asynchronously.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        let sem = &mut self.inner.sem;
        let finish_sem = &mut self.inner.finish_sem;
        blocker(move || match sem.try_wait() {
            Ok(()) => Ok(true),
            Err(err) if err.raw_os_error() == Some(EAGAIN) => Ok(finish_sem.counter()? > 0),
            Err(err) => Err(err),
        })
        .await
        .ok()?;
        self.read_one()
    }

    pub fn empty(&self) -> bool {
        self.inner.head().load(Ordering::Relaxed) == self.inner.tail().load(Ordering::Acquire)
    }

    pub fn dropped_count(&self) -> usize {
        self.inner.dropped().load(Ordering::Relaxed)
    }

    pub fn drop_event(&self) -> DropEvent {
        DropEvent {
            total: self.dropped_count(),
        }
    }

    fn read_one(&mut self) -> Option<Vec<u8>> {
        let head = self.inner.head().load(Ordering::Relaxed);
        let tail = self.inner.tail().load(Ordering::Acquire);
        if head == tail {
            return None;
        }

        let word = std::mem::size_of::<usize>();
        let mut size_bytes = [0_u8; std::mem::size_of::<usize>()];
        unsafe { self.inner.read_wrapped(head, &mut size_bytes) };
        let size = usize::from_ne_bytes(size_bytes);
        let padded_len = size.checked_add(word - 1)? & !(word - 1);
        let record_len = word.checked_add(padded_len)?;
        if record_len > tail.wrapping_sub(head) || record_len > self.inner.size {
            return None;
        }

        let mut data = vec![0_u8; size];
        unsafe { self.inner.read_wrapped(head.wrapping_add(word), &mut data) };
        self.inner
            .head()
            .store(head.wrapping_add(record_len), Ordering::Release);
        Some(data)
    }
}

// Each endpoint owns its mapping and moves only byte buffers across threads.
// Neither endpoint is Sync, so one endpoint cannot be used concurrently.
unsafe impl Send for Receiver {}
unsafe impl Send for Sender {}

#[cfg(test)]
mod tests {
    use super::{Receiver, Sender};
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };

    static NEXT_NAME: AtomicUsize = AtomicUsize::new(0);

    fn name(label: &str) -> String {
        format!(
            "/miniperf_{label}_{}_{}",
            std::process::id(),
            NEXT_NAME.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn rejects_non_power_of_two_capacity() {
        let err = match Sender::new(&name("bad_capacity"), 1000) {
            Ok(_) => panic!("non-power-of-two capacity was accepted"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn attach_preserves_queued_data() {
        let name = name("attach");
        let sender = Sender::new(&name, 64).unwrap();
        sender.send_sync(&42_u64.to_le_bytes()).unwrap();
        let mut receiver = unsafe { Receiver::attach(&name, 64).unwrap() };
        assert_eq!(receiver.recv_sync(), Some(42_u64.to_le_bytes().to_vec()));
    }

    #[test]
    fn wraps_header_and_payload() {
        let name = name("wrap");
        let sender = Sender::new(&name, 64).unwrap();
        let mut receiver = unsafe { Receiver::attach(&name, 64).unwrap() };
        for value in 0..20_u8 {
            sender.send_sync(&[value; 9]).unwrap();
            assert_eq!(receiver.recv_sync(), Some(vec![value; 9]));
        }
        assert_eq!(sender.dropped_count(), 0);
    }

    #[test]
    fn full_ring_drops_and_reports_counter() {
        let name = name("drops");
        let sender = Sender::new(&name, 32).unwrap();
        let mut receiver = unsafe { Receiver::attach(&name, 32).unwrap() };
        sender.send_sync(&1_u64.to_le_bytes()).unwrap();
        sender.send_sync(&2_u64.to_le_bytes()).unwrap();
        sender.send_sync(&3_u64.to_le_bytes()).unwrap();
        assert_eq!(sender.drop_event().total, 1);
        assert_eq!(receiver.dropped_count(), 1);
        assert_eq!(receiver.recv_sync(), Some(1_u64.to_le_bytes().to_vec()));
        assert_eq!(receiver.recv_sync(), Some(2_u64.to_le_bytes().to_vec()));
    }

    #[test]
    fn duplicate_creation_does_not_reset_live_channel() {
        let name = name("duplicate");
        let sender = Sender::new(&name, 64).unwrap();
        sender.send_sync(b"queued").unwrap();
        let error = match Sender::new(&name, 64) {
            Ok(_) => panic!("duplicate channel was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let mut receiver = unsafe { Receiver::attach(&name, 64).unwrap() };
        assert_eq!(receiver.try_recv(), Some(b"queued".to_vec()));
    }

    #[test]
    fn attachment_rejects_wrong_mapping_extent() {
        let name = name("extent");
        let _sender = Sender::new(&name, 64).unwrap();
        let error = match unsafe { Receiver::attach(&name, 128) } {
            Ok(_) => panic!("wrong mapping extent was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn receiver_keeps_semaphore_and_mapping_after_creator_drops() {
        let name = name("receiver_lifetime");
        let sender = Sender::new(&name, 64).unwrap();
        let mut receiver = unsafe { Receiver::attach(&name, 64).unwrap() };
        sender.send_sync(b"retained").unwrap();
        drop(sender);
        assert_eq!(receiver.recv_sync(), Some(b"retained".to_vec()));
        assert_eq!(receiver.recv_sync(), None);
    }

    #[test]
    fn sender_keeps_semaphore_and_mapping_after_creator_drops() {
        let name = name("sender_lifetime");
        let receiver = Receiver::new(&name, 64).unwrap();
        let sender = unsafe { Sender::attach(&name, 64).unwrap() };
        drop(receiver);
        sender.send_sync(b"retained").unwrap();
        assert_eq!(sender.dropped_count(), 0);
    }

    #[test]
    fn received_bytes_remain_owned_when_ring_wraps() {
        let name = name("owned");
        let sender = Sender::new(&name, 64).unwrap();
        let mut receiver = unsafe { Receiver::attach(&name, 64).unwrap() };
        sender.send_sync(b"first").unwrap();
        let first = receiver.try_recv().unwrap();
        for _ in 0..10 {
            sender.send_sync(b"replacement").unwrap();
            assert_eq!(receiver.try_recv(), Some(b"replacement".to_vec()));
        }
        assert_eq!(first, b"first");
    }

    #[test]
    fn async_receive_future_can_move_between_threads() {
        fn require_send(_: impl Send) {}
        let mut receiver = Receiver::new(&name("future"), 64).unwrap();
        require_send(receiver.recv());
    }

    #[test]
    fn concurrent_spsc_stress_preserves_order() {
        const COUNT: usize = 100_000;
        let name = name("stress");
        let mut receiver = Receiver::new(&name, 1 << 16).unwrap();
        let sender = unsafe { Sender::attach(&name, 1 << 16).unwrap() };

        let producer = std::thread::spawn(move || {
            let mut sent = 0_usize;
            while sent < COUNT {
                let before = sender.dropped_count();
                sender.send_sync(&sent.to_le_bytes()).unwrap();
                if sender.dropped_count() == before {
                    sent += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        for expected in 0..COUNT {
            assert_eq!(receiver.recv_sync(), Some(expected.to_le_bytes().to_vec()));
        }
        producer.join().unwrap();
    }

    /// Run explicitly when evaluating the Phase 1 throughput acceptance gate.
    #[test]
    #[ignore = "throughput benchmark; run with --release --ignored"]
    fn throughput_single_pair_exceeds_one_million_events_per_second() {
        const COUNT: usize = 1_000_000;
        let name = name("throughput");
        let mut receiver = Receiver::new(&name, 1 << 20).unwrap();
        let sender = unsafe { Sender::attach(&name, 1 << 20).unwrap() };
        let start = Instant::now();
        let producer = std::thread::spawn(move || {
            let mut sent = 0_usize;
            while sent < COUNT {
                let before = sender.dropped_count();
                sender.send_sync(&sent.to_le_bytes()).unwrap();
                sent += usize::from(sender.dropped_count() == before);
            }
        });
        for expected in 0..COUNT {
            assert_eq!(receiver.recv_sync(), Some(expected.to_le_bytes().to_vec()));
        }
        producer.join().unwrap();
        let rate = COUNT as f64 / start.elapsed().as_secs_f64();
        assert!(rate > 1_000_000.0, "throughput was {rate:.0} events/s");
    }
}
