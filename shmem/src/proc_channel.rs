use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind},
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{platform, utils::blocker};

pub trait Sendable {
    fn as_raw_bytes(&self) -> Vec<u8>;
    fn from_raw_bytes(bytes: &[u8]) -> Self;
}

pub struct Sender<T: Sendable> {
    inner: Inner,
    phantom: PhantomData<T>,
}

pub struct Receiver<T: Sendable> {
    inner: Inner,
    phantom: PhantomData<T>,
}

/// A snapshot of the channel's backpressure counter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DropEvent {
    /// Total messages dropped since the channel was created.
    pub total: usize,
}

struct Inner {
    shmem: platform::Shmem,
    sem: platform::Semaphore,
    finish_sem: platform::Semaphore,
    ptr: *mut (),
    head: *mut usize,
    tail: *mut usize,
    dropped: *mut usize,
    size: usize,
}

impl Inner {
    fn semaphore_slot_size() -> usize {
        platform::Semaphore::required_size().next_multiple_of(std::mem::align_of::<usize>())
    }

    fn semaphore_name(channel_name: &str, suffix: char) -> String {
        let mut hasher = DefaultHasher::new();
        channel_name.hash(&mut hasher);
        format!("/mp_{:016x}_{suffix}", hasher.finish())
    }

    fn data_offset() -> usize {
        let metadata_size = 2 * Self::semaphore_slot_size() + 3 * std::mem::size_of::<usize>();
        metadata_size.next_multiple_of(std::mem::align_of::<usize>())
    }

    fn compute_size(data_size: usize) -> usize {
        Self::data_offset() + data_size
    }

    fn new(shmem: platform::Shmem, data_size: usize, init: bool) -> Result<Self, Error> {
        if !data_size.is_power_of_two() || data_size < 2 * std::mem::size_of::<usize>() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "shared-memory ring capacity must be a power of two and hold one record",
            ));
        }

        let sem_name = Self::semaphore_name(shmem.name(), 'r');
        let finish_sem_name = Self::semaphore_name(shmem.name(), 'f');
        let (sem, finish_sem) = if init {
            (
                platform::Semaphore::create(shmem.as_mut_ptr(), &sem_name)?,
                platform::Semaphore::create(
                    unsafe { shmem.as_mut_ptr().byte_add(Self::semaphore_slot_size()) },
                    &finish_sem_name,
                )?,
            )
        } else {
            (
                platform::Semaphore::open(shmem.as_mut_ptr(), &sem_name)?,
                platform::Semaphore::open(
                    unsafe { shmem.as_mut_ptr().byte_add(Self::semaphore_slot_size()) },
                    &finish_sem_name,
                )?,
            )
        };

        let sem_size = 2 * Self::semaphore_slot_size();
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

impl<T: Sendable> Sender<T> {
    pub fn new(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::create(name, Inner::compute_size(data_size))?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, true)?,
            phantom: PhantomData,
        })
    }

    pub fn attach(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::open(name, Inner::compute_size(data_size))?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, false)?,
            phantom: PhantomData,
        })
    }

    pub fn name(&self) -> &str {
        self.inner.shmem.name()
    }

    /// Sends a message, or drops it and increments the backpressure counter if full.
    pub fn send_sync(&self, object: T) -> Result<(), Error> {
        let data = object.as_raw_bytes();
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
            self.inner.write_wrapped(tail.wrapping_add(word), &data);
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

impl<T: Sendable> Drop for Sender<T> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl<T: Sendable> Receiver<T> {
    pub fn attach(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::open(name, Inner::compute_size(data_size))?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, false)?,
            phantom: PhantomData,
        })
    }

    pub fn new(name: &str, data_size: usize) -> Result<Self, Error> {
        let shmem = platform::Shmem::create(name, Inner::compute_size(data_size))?;
        Ok(Self {
            inner: Inner::new(shmem, data_size, true)?,
            phantom: PhantomData,
        })
    }

    pub fn recv_sync(&self) -> Option<T> {
        self.inner.sem.wait().ok()?;
        self.read_one()
    }

    pub async fn recv(&self) -> Option<T> {
        blocker(|| match self.inner.sem.try_wait() {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                Ok(self.inner.finish_sem.counter()? > 0)
            }
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

    fn read_one(&self) -> Option<T> {
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
        Some(T::from_raw_bytes(&data))
    }
}

impl<T: Copy> Sendable for T {
    fn as_raw_bytes(&self) -> Vec<u8> {
        let mut vec = vec![0; std::mem::size_of::<Self>()];
        unsafe {
            std::ptr::copy_nonoverlapping(
                (self as *const Self).cast::<u8>(),
                vec.as_mut_ptr(),
                std::mem::size_of::<Self>(),
            );
        }
        vec
    }

    fn from_raw_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() >= std::mem::size_of::<T>());
        let mut to = MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                to.as_mut_ptr().cast::<u8>(),
                std::mem::size_of::<T>(),
            );
            to.assume_init()
        }
    }
}

// The protocol requires exactly one producer and one consumer. Moving either endpoint
// to another thread is safe. Receiver is Sync so its borrowed async receive future can
// move between executor threads; callers must still serialize receive operations.
unsafe impl<T: Sendable> Send for Receiver<T> {}
unsafe impl<T: Sendable> Sync for Receiver<T> {}
unsafe impl<T: Sendable> Send for Sender<T> {}

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
        let err = match Sender::<u64>::new(&name("bad_capacity"), 1000) {
            Ok(_) => panic!("non-power-of-two capacity was accepted"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn attach_preserves_queued_data() {
        let name = name("attach");
        let sender = Sender::<u64>::new(&name, 64).unwrap();
        sender.send_sync(42).unwrap();
        let receiver = Receiver::<u64>::attach(&name, 64).unwrap();
        assert_eq!(receiver.recv_sync(), Some(42));
    }

    #[test]
    fn wraps_header_and_payload() {
        let name = name("wrap");
        let sender = Sender::<[u8; 9]>::new(&name, 64).unwrap();
        let receiver = Receiver::<[u8; 9]>::attach(&name, 64).unwrap();
        for value in 0..20_u8 {
            sender.send_sync([value; 9]).unwrap();
            assert_eq!(receiver.recv_sync(), Some([value; 9]));
        }
        assert_eq!(sender.dropped_count(), 0);
    }

    #[test]
    fn full_ring_drops_and_reports_counter() {
        let name = name("drops");
        let sender = Sender::<u64>::new(&name, 32).unwrap();
        let receiver = Receiver::<u64>::attach(&name, 32).unwrap();
        sender.send_sync(1).unwrap();
        sender.send_sync(2).unwrap();
        sender.send_sync(3).unwrap();
        assert_eq!(sender.drop_event().total, 1);
        assert_eq!(receiver.dropped_count(), 1);
        assert_eq!(receiver.recv_sync(), Some(1));
        assert_eq!(receiver.recv_sync(), Some(2));
    }

    #[test]
    fn concurrent_spsc_stress_preserves_order() {
        const COUNT: usize = 100_000;
        let name = name("stress");
        let receiver = Receiver::<usize>::new(&name, 1 << 16).unwrap();
        let sender = Sender::<usize>::attach(&name, 1 << 16).unwrap();

        let producer = std::thread::spawn(move || {
            let mut sent = 0;
            while sent < COUNT {
                let before = sender.dropped_count();
                sender.send_sync(sent).unwrap();
                if sender.dropped_count() == before {
                    sent += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        for expected in 0..COUNT {
            assert_eq!(receiver.recv_sync(), Some(expected));
        }
        producer.join().unwrap();
    }

    /// Run explicitly when evaluating the Phase 1 throughput acceptance gate.
    #[test]
    #[ignore = "throughput benchmark; run with --release --ignored"]
    fn throughput_single_pair_exceeds_one_million_events_per_second() {
        const COUNT: usize = 1_000_000;
        let name = name("throughput");
        let receiver = Receiver::<usize>::new(&name, 1 << 20).unwrap();
        let sender = Sender::<usize>::attach(&name, 1 << 20).unwrap();
        let start = Instant::now();
        let producer = std::thread::spawn(move || {
            let mut sent = 0;
            while sent < COUNT {
                let before = sender.dropped_count();
                sender.send_sync(sent).unwrap();
                sent += usize::from(sender.dropped_count() == before);
            }
        });
        for expected in 0..COUNT {
            assert_eq!(receiver.recv_sync(), Some(expected));
        }
        producer.join().unwrap();
        let rate = COUNT as f64 / start.elapsed().as_secs_f64();
        assert!(rate > 1_000_000.0, "throughput was {rate:.0} events/s");
    }
}
