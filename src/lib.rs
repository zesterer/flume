//! # Flume
//!
//! A blazingly fast multi-producer, single-consumer channel.
//!
//! *"Do not communicate by sharing memory; instead, share memory by communicating."*
//!
//! ## Examples
//!
//! ```
//! let (tx, rx) = flume::unbounded();
//!
//! tx.send(42).unwrap();
//! assert_eq!(rx.recv().unwrap(), 42);
//! ```

#[cfg(feature = "select")]
pub mod select;
#[cfg(feature = "async")]
pub mod r#async;

mod signal;

// Reexports
#[cfg(feature = "select")]
pub use select::Selector;

use std::{
    collections::VecDeque,
    sync::{self, Arc, Condvar, Mutex, MutexGuard, WaitTimeoutResult, atomic::{AtomicUsize, AtomicBool, Ordering, spin_loop_hint}},
    time::{Duration, Instant},
    marker::PhantomData,
    thread,
    fmt,
};

// #[cfg(windows)]
// use std::sync::{Mutex as InnerMutex, MutexGuard as InnerMutexGuard};
// #[cfg(not(windows))]
// use spin::{Mutex as InnerMutex, MutexGuard as InnerMutexGuard};

use spinning_top::{Spinlock, SpinlockGuard};

#[cfg(feature = "async")]
use std::task::Waker;
#[cfg(feature = "select")]
use crate::select::Token;
#[cfg(feature = "async")]
use crate::r#async::RecvFuture;

/// An error that may be emitted when attempting to send a value into a channel on a sender.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "sending on a closed channel".fmt(f)
    }
}

impl<T> std::error::Error for SendError<T> where T: fmt::Debug {}

/// An error that may be emitted when attempting to send a value into a channel on a sender.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Disconnected => "receiving on a closed channel".fmt(f),
        }
    }
}

impl std::error::Error for RecvError {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendTimeoutError<T> {
    Timeout(T),
    Disconnected(T),
}

impl<T> fmt::Display for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendTimeoutError::Timeout(..) => "timed out sending on a full channel".fmt(f),
            SendTimeoutError::Disconnected(..) => "sending on a closed channel".fmt(f),
        }
    }
}

impl<T> std::error::Error for SendTimeoutError<T> where T: fmt::Debug {}

pub enum TrySendTimeoutError<T> {
    Full(T),
    Disconnected(T),
    Timeout(T),
}

/// An error that may be emitted when attempting to wait for a value on a receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(..) => "sending on a full channel".fmt(f),
            TrySendError::Disconnected(..) => "sending on a closed channel".fmt(f),
        }
    }
}

impl<T> std::error::Error for TrySendError<T> where T: fmt::Debug {}

/// An error that may be emitted when attempting to fetch a value on a receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryRecvError::Empty => "receiving on an empty channel".fmt(f),
            TryRecvError::Disconnected => "channel is empty and closed".fmt(f),
        }
    }
}

impl std::error::Error for TryRecvError {}

/// An error that may be emitted when attempting to wait for a value on a receiver with a timeout.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

impl fmt::Display for RecvTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvTimeoutError::Timeout => "timed out waiting on a channel".fmt(f),
            RecvTimeoutError::Disconnected => "channel is empty and closed".fmt(f),
        }
    }
}

impl std::error::Error for RecvTimeoutError {}

pub enum TryRecvTimeoutError {
    Empty,
    Timeout,
    Disconnected,
}

pub struct Slot<T, S: ?Sized>(Spinlock<Option<T>>, S);

impl<T, S: ?Sized + signal::Signal> Slot<T, S> {
    pub fn new(item: Option<T>) -> Arc<Self> where S: Default {
        Arc::new(Self(Spinlock::new(item), S::default()))
    }

    pub fn signal(&self) -> &S {
        &self.1
    }

    pub fn fire_recv(&self) -> T {
        let item = self.0.lock().take().unwrap();
        self.1.fire();
        item
    }

    pub fn fire_send(&self, item: T) {
        *self.0.lock() = Some(item);
        self.1.fire();
    }
}

impl<T> Slot<T, signal::SyncSignal> {
    pub fn wait_recv(&self, abort: &AtomicBool) -> Option<T> {
        loop {
            let item = self.0.lock().take();
            if let Some(item) = item {
                break Some(item);
            } else if abort.load(Ordering::SeqCst) {
                break None;
            } else {
                self.1.wait()
            }
        }
    }

    // Err(true) if timeout
    pub fn wait_deadline_recv(&self, abort: &AtomicBool, deadline: Instant) -> Result<T, bool> {
        loop {
            let item = self.0.lock().take();
            if let Some(item) = item {
                break Ok(item);
            } else if abort.load(Ordering::SeqCst) {
                break Err(false);
            } else if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                self.1.wait_timeout(dur);
            } else {
                break Err(true);
            }
        }
    }

    pub fn wait_send(&self, abort: &AtomicBool) {
        loop {
            if self.0.lock().is_none() {
                break;
            } else if abort.load(Ordering::SeqCst) {
                break;
            }

            self.1.wait();
        }
    }

    // Err(true) if timeout
    pub fn wait_deadline_send(&self, abort: &AtomicBool, deadline: Instant) -> Result<(), bool> {
        loop {
            if self.0.lock().is_none() {
                break Ok(());
            } else if abort.load(Ordering::SeqCst) {
                break Err(false);
            } else if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                self.1.wait_timeout(dur);
            } else {
                break Err(true);
            }
        }
    }
}

#[inline]
#[cfg(not(windows))]
fn wait_lock<'a, T>(lock: &'a Spinlock<T>) -> SpinlockGuard<'a, T> {
    let mut i = 0;
    loop {
        for _ in 0..5 {
            if let Some(guard) = lock.try_lock() {
                return guard;
            }
            thread::yield_now();
        }
        thread::sleep(Duration::from_nanos(i * 50));
        i += 1;
    }
}

// #[inline]
// #[cfg(windows)]
// fn wait_lock<'a, T>(lock: &'a InnerMutex<T>) -> InnerMutexGuard<'a, T> {
//     lock.lock().unwrap()
// }

struct Bounded<T> {
    sending: VecDeque<Arc<Slot<T, dyn signal::Signal + Send + Sync>>>,
    queue: VecDeque<T>,
    waiting: VecDeque<Arc<Slot<T, dyn signal::Signal + Send + Sync>>>,
}

impl<T> Bounded<T> {
    fn pull_pending(&mut self, effective_cap: usize) {
        while self.queue.len() < effective_cap {
            if let Some(s) = self.sending.pop_front() {
                self.queue.push_back(s.fire_recv());
            } else {
                break;
            }
        }
    }
}

struct Unbounded<T> {
    queue: VecDeque<T>,
    waiting: VecDeque<Arc<Slot<T, dyn signal::Signal + Send + Sync>>>,
}

enum Chan<T> {
    Bounded(usize, Spinlock<Bounded<T>>),
    Unbounded(Spinlock<Unbounded<T>>),
}

enum Block {
    None,
    Until(Instant),
    Forever,
}

struct Shared<T> {
    chan: Chan<T>,
    disconnected: AtomicBool,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Shared<T> {
    fn new(cap: Option<usize>) -> Self {
        Self {
            chan: match cap {
                Some(cap) => Chan::Bounded(cap, Spinlock::new(Bounded {
                    sending: VecDeque::new(),
                    queue: VecDeque::new(),
                    waiting: VecDeque::new(),
                })),
                None => Chan::Unbounded(Spinlock::new(Unbounded {
                    queue: VecDeque::new(),
                    waiting: VecDeque::new(),
                })),
            },
            disconnected: AtomicBool::new(false),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),
        }
    }

    fn send(&self, item: T, block: Option<Option<Instant>>) -> Result<(), TrySendTimeoutError<T>> {
        match &self.chan {
            Chan::Bounded(cap, inner) => {
                let mut inner_guard = wait_lock(inner);

                if self.disconnected.load(Ordering::SeqCst) {
                    Err(TrySendTimeoutError::Disconnected(item))
                } else if let Some(r) = inner_guard.waiting.pop_front() {
                    debug_assert!(inner_guard.queue.len() == 0);
                    r.fire_send(item);
                    Ok(())
                } else if inner_guard.queue.len() < *cap {
                    inner_guard.queue.push_back(item);
                    Ok(())
                } else if let Some(deadline) = block {
                    let slot = Slot::<_, signal::SyncSignal>::new(Some(item));
                    inner_guard.sending.push_back(slot.clone());
                    drop(inner_guard);

                    if let Some(deadline) = deadline {
                        slot.wait_deadline_send(&self.disconnected, deadline)
                            .or_else(|timed_out| {
                                if timed_out { // Remove our signal
                                    let slot: Arc<Slot<T, dyn signal::Signal + Send + Sync>> = slot.clone();
                                    wait_lock(inner).sending.retain(|s| !Arc::ptr_eq(s, &slot));
                                }
                                let item = slot.0.lock().take();
                                item.map(|item| if self.disconnected.load(Ordering::Relaxed) {
                                    Err(TrySendTimeoutError::Disconnected(item))
                                } else {
                                    Err(TrySendTimeoutError::Timeout(item))
                                })
                                .unwrap_or(Ok(()))
                            })
                    } else {
                        slot.wait_send(&self.disconnected);

                        let item = slot.0.lock().take();
                        match item {
                            Some(item) => Err(TrySendTimeoutError::Disconnected(item)),
                            None => Ok(()),
                        }
                    }
                } else {
                    Err(TrySendTimeoutError::Full(item))
                }
            },
            Chan::Unbounded(inner) => {
                let mut inner = wait_lock(inner);
                if self.disconnected.load(Ordering::SeqCst) {
                    return Err(TrySendTimeoutError::Disconnected(item))
                } else if let Some(r) = inner.waiting.pop_front() {
                    drop(inner);
                    r.fire_send(item);
                } else {
                    inner.queue.push_back(item);
                }

                Ok(())
            },
        }
    }

    fn recv(&self, block: Option<Option<Instant>>) -> Result<T, TryRecvTimeoutError> {
        match &self.chan {
            Chan::Bounded(cap, inner) => {
                let mut inner_guard = wait_lock(inner);
                inner_guard.pull_pending(*cap + 1); // Effective cap of cap + 1 because we're going to be popping one

                if let Some(item) = inner_guard.queue.pop_front() {
                    Ok(item)
                } else if self.disconnected.load(Ordering::SeqCst) {
                    Err(TryRecvTimeoutError::Disconnected)
                } else if let Some(deadline) = block {
                    let slot = Slot::<_, signal::SyncSignal>::new(None);
                    inner_guard.waiting.push_back(slot.clone());
                    drop(inner_guard);

                    if let Some(deadline) = deadline {
                        slot.wait_deadline_recv(&self.disconnected, deadline)
                            .or_else(|timed_out| {
                                if timed_out { // Remove our signal
                                    let slot: Arc<Slot<T, dyn signal::Signal + Send + Sync>> = slot.clone();
                                    wait_lock(inner).waiting.retain(|s| !Arc::ptr_eq(s, &slot));
                                }
                                let item = slot.0.lock().take();
                                item.ok_or_else(|| if self.disconnected.load(Ordering::Relaxed) {
                                    TryRecvTimeoutError::Disconnected
                                } else {
                                    TryRecvTimeoutError::Timeout
                                })
                            })
                    } else {
                        slot.wait_recv(&self.disconnected)
                            .ok_or(TryRecvTimeoutError::Disconnected)
                    }
                } else {
                    Err(TryRecvTimeoutError::Empty)
                }
            },
            Chan::Unbounded(inner) => {
                let mut inner_guard = wait_lock(inner);
                if let Some(item) = inner_guard.queue.pop_front() {
                    Ok(item)
                } else if self.disconnected.load(Ordering::SeqCst) {
                    Err(TryRecvTimeoutError::Disconnected)
                } else if let Some(deadline) = block {
                    let slot = Slot::<_, signal::SyncSignal>::new(None);
                    inner_guard.waiting.push_back(slot.clone());
                    drop(inner_guard);

                    if let Some(deadline) = deadline {
                        slot.wait_deadline_recv(&self.disconnected, deadline)
                            .or_else(|timed_out| {
                                if timed_out { // Remove our signal
                                    let slot: Arc<Slot<T, dyn signal::Signal + Send + Sync>> = slot.clone();
                                    wait_lock(inner).waiting.retain(|s| !Arc::ptr_eq(s, &slot));
                                }
                                let item = slot.0.lock().take();
                                item.ok_or_else(|| if self.disconnected.load(Ordering::Relaxed) {
                                    TryRecvTimeoutError::Disconnected
                                } else {
                                    TryRecvTimeoutError::Timeout
                                })
                            })
                    } else {
                        slot.wait_recv(&self.disconnected)
                            .ok_or(TryRecvTimeoutError::Disconnected)
                    }
                } else {
                    Err(TryRecvTimeoutError::Empty)
                }
            },
        }
    }

    /// Disconnect anything listening on this channel (this will not prevent receivers receiving
    /// items that have already been sent)
    fn disconnect_all(&self) {
        self.disconnected.store(true, Ordering::Relaxed);

        match &self.chan {
            Chan::Bounded(cap, inner) => {
                let mut inner = inner.lock();
                inner.pull_pending(*cap);
                inner.sending.iter().for_each(|slot| slot.signal().fire());
                inner.waiting.iter().for_each(|slot| slot.signal().fire());
            },
            Chan::Unbounded(inner) => inner
                .lock()
                .waiting
                .iter()
                .for_each(|slot| slot.signal().fire()),
        }
    }

    fn is_empty_inner(&self) -> bool {
        self.len_inner() == 0
    }

    fn is_full_inner(&self) -> bool {
        self.capacity_inner().map(|cap| cap == self.len_inner()).unwrap_or(false)
    }

    fn len_inner(&self) -> usize {
        match &self.chan {
            Chan::Bounded(cap, inner) => {
                let mut inner = inner.lock();
                inner.pull_pending(*cap);
                inner.queue.len()
            },
            Chan::Unbounded(inner) => inner.lock().queue.len(),
        }
    }

    fn capacity_inner(&self) -> Option<usize> {
        match &self.chan {
            Chan::Bounded(cap, _) => Some(*cap),
            Chan::Unbounded(_) => None,
        }
    }
}

/// A transmitting end of a channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped. If the channel is bounded and is full, this method will block.
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.shared.send(msg, Some(None)).map_err(|err| match err {
            TrySendTimeoutError::Disconnected(msg) => SendError(msg),
            _ => unreachable!(),
        })
    }

    /// Attempt to send a value into the channel. If the channel is bounded and full, or the
    /// receiver has been dropped, an error is returned. If the channel associated with this
    /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.shared.send(msg, None).map_err(|err| match err {
            TrySendTimeoutError::Full(msg) => TrySendError::Full(msg),
            TrySendTimeoutError::Disconnected(msg) => TrySendError::Disconnected(msg),
            _ => unreachable!(),
        })
    }

    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped or the deadline has passed. If the channel is bounded and is full, this method
    /// will block.
    pub fn send_deadline(&self, msg: T, deadline: Instant) -> Result<(), SendTimeoutError<T>> {
        self.shared.send(msg, Some(Some(deadline))).map_err(|err| match err {
            TrySendTimeoutError::Disconnected(msg) => SendTimeoutError::Disconnected(msg),
            TrySendTimeoutError::Timeout(msg) => SendTimeoutError::Timeout(msg),
            _ => unreachable!(),
        })
    }

    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped or the timeout has expired. If the channel is bounded and is full, this method
    /// will block.
    pub fn send_timeout(&self, msg: T, dur: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_deadline(msg, Instant::now().checked_add(dur).unwrap())
    }

    /// Returns true if the channel is empty.
    /// Note: Zero-capacity channels are always empty.
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty_inner()
    }

    /// Returns true if the channel is full.
    /// Note: Zero-capacity channels are always full.
    pub fn is_full(&self) -> bool {
        self.shared.is_full_inner()
    }

    /// Returns the number of messages in the channel
    pub fn len(&self) -> usize {
        self.shared.len_inner()
    }

    /// If the channel is bounded, returns its capacity.
    pub fn capacity(&self) -> Option<usize> {
        self.shared.capacity_inner()
    }
}

impl<T> Clone for Sender<T> {
    /// Clone this sender. [`Sender`] acts as a handle to the ending a channel. Remaining channel
    /// contents will only be cleaned up when all senders and the receiver have been dropped.
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Notify receivers that all senders have been dropped if the number of senders drops to 0.
        if self.shared.sender_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.disconnect_all();
        }
    }
}

/// The receiving end of a channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Attempt to fetch an incoming value from the channel associated with this receiver,
    /// returning an error if the channel is empty or all channel senders have been dropped.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.shared.recv(None).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => TryRecvError::Disconnected,
            TryRecvTimeoutError::Empty => TryRecvError::Empty,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv(Some(None)).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => RecvError::Disconnected,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv(Some(Some(deadline))).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => RecvTimeoutError::Disconnected,
            TryRecvTimeoutError::Timeout => RecvTimeoutError::Timeout,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the timeout has expired.
    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_deadline(Instant::now().checked_add(dur).unwrap())
    }

    // Takes `&mut self` to avoid >1 task waiting on this channel
    // TODO: Is this necessary?
    /// Create a future that may be used to wait asynchronously for an incoming value on the
    /// channel associated with this receiver.
    #[cfg(feature = "async")]
    pub fn recv_async(&mut self) -> RecvFuture<T> {
        RecvFuture::new(self)
    }

    /// A blocking iterator over the values received on the channel that finishes iteration when
    /// all receivers of the channel have been dropped.
    pub fn iter(&self) -> Iter<T> {
        Iter { receiver: &self }
    }

    /// A non-blocking iterator over the values received on the channel that finishes iteration
    /// when all receivers of the channel have been dropped or the channel is empty.
    pub fn try_iter(&self) -> TryIter<T> {
        TryIter { receiver: &self }
    }

    /// Take all items currently sitting in the channel and produce an iterator over them. Unlike
    /// `try_iter`, the iterator will not attempt to fetch any more values from the channel once
    /// the function has been called.
    pub fn drain(&self) -> Drain<T> {
        let queue = match &self.shared.chan {
            // Rendezvous is suppose to be an instant handover, so it doesn't make conceptual sense
            // for the channel to have items sitting in it
            // Channel::Rendezvous { .. } => VecDeque::new(),
            Chan::Bounded(cap, inner) => {
                let mut inner = inner.lock();
                inner.pull_pending(*cap);
                std::mem::take(&mut inner.queue)
            },
            Chan::Unbounded(inner) => std::mem::take(&mut inner.lock().queue),
        };

        Drain { queue, _phantom: PhantomData }
    }

    /// Returns true if the channel is empty.
    /// Note: Zero-capacity channels are always empty.
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty_inner()
    }

    /// Returns true if the channel is full.
    /// Note: Zero-capacity channels are always full.
    pub fn is_full(&self) -> bool {
        self.shared.is_full_inner()
    }

    /// Returns the number of messages in the channel.
    pub fn len(&self) -> usize {
        self.shared.len_inner()
    }

    /// If the channel is bounded, returns its capacity.
    pub fn capacity(&self) -> Option<usize> {
        self.shared.capacity_inner()
    }
}

impl<T> Clone for Receiver<T> {
    /// Clone this receiver. [`Receiver`] acts as a handle to the ending a channel. Remaining
    /// channel contents will only be cleaned up when all senders and the receiver have been
    /// dropped.
    fn clone(&self) -> Self {
        self.shared.receiver_count.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Notify senders that all receivers have been dropped if the number of receivers drops
        // to 0.
        if self.shared.receiver_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.disconnect_all();
        }
    }
}

impl<'a, T> IntoIterator for &'a Receiver<T> {
    type Item = T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        Iter { receiver: self }
    }
}

impl<T> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { receiver: self }
    }
}

/// An iterator over the items received from a channel.
pub struct Iter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}

/// An non-blocking iterator over the items received from a channel.
pub struct TryIter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.try_recv().ok()
    }
}

/// An fixed-sized iterator over the items drained from a channel.
#[derive(Debug)]
pub struct Drain<'a, T> {
    queue: VecDeque<T>,
    /// A phantom field used to constrain the lifetime of this iterator. We do this because the
    /// implementation may change and we don't want to unintentionally constrain it. Removing this
    /// lifetime later is a possibility.
    _phantom: PhantomData<&'a ()>,
}

impl<'a, T> Iterator for Drain<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop_front()
    }
}

impl<'a, T> ExactSizeIterator for Drain<'a, T> {
    fn len(&self) -> usize {
        self.queue.len()
    }
}

/// An owned iterator over the items received from a channel.
pub struct IntoIter<T> {
    receiver: Receiver<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}

/// Create a channel with no maximum capacity.
///
/// Create an unbounded channel with a [`Sender`] and [`Receiver`] connected to each end
/// respectively. Values sent in one end of the channel will be received on the other end. The
/// channel is thread-safe, and both sender and receiver may be sent to threads as necessary. In
/// addition, [`Sender`] may be cloned.
///
/// # Examples
/// ```
/// let (tx, rx) = flume::unbounded();
///
/// tx.send(42).unwrap();
/// assert_eq!(rx.recv().unwrap(), 42);
/// ```
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared::new(None));
    (
        Sender { shared: shared.clone() },
        Receiver { shared },
    )
}

/// Create a channel with a maximum capacity.
///
/// Create a bounded channel with a [`Sender`] and [`Receiver`] connected to each end
/// respectively. Values sent in one end of the channel will be received on the other end. The
/// channel is thread-safe, and both sender and receiver may be sent to threads as necessary. In
/// addition, [`Sender`] may be cloned.
///
/// Unlike an [`unbounded`] channel, if there is no space left for new messages, calls to
/// [`Sender::send`] will block (unblocking once a receiver has made space). If blocking behaviour
/// is not desired, [`Sender::try_send`] may be used.
///
/// Like `std::sync::mpsc`, `flume` supports 'rendezvous' channels. A bounded queue with a maximum
/// capacity of zero will block senders until a receiver is available to take the value.
///
/// # Examples
/// ```
/// let (tx, rx) = flume::bounded(32);
///
/// for i in 1..33 {
///     tx.send(i).unwrap();
/// }
/// assert!(tx.try_send(33).is_err());
///
/// assert_eq!(rx.try_iter().sum::<u32>(), (1..33).sum());
/// ```
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared::new(Some(cap)));
    (
        Sender { shared: shared.clone() },
        Receiver { shared },
    )
}
