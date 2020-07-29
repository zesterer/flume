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
use crate::signal::{Signal, SyncSignal};

#[cfg(feature = "select")]
use crate::select::Token;

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

// TODO: Investigate some sort of invalidation flag for timeouts
pub struct Slot<T, S: ?Sized>(Spinlock<Option<T>>, S);

impl<T, S: ?Sized + Signal> Slot<T, S> {
    pub fn new(item: Option<T>, signal: S) -> Arc<Self> where S: Sized {
        Arc::new(Self(Spinlock::new(item), signal))
    }

    pub fn signal(&self) -> &S {
        &self.1
    }

    pub fn fire_recv(&self) -> T {
        let item = self.0.lock().take().unwrap();
        self.signal().fire();
        item
    }

    pub fn fire_send(&self, item: T) {
        *self.0.lock() = Some(item);
        self.signal().fire();
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().is_none()
    }

    pub fn try_take(&self) -> Option<T> {
        self.0.lock().take()
    }
}

impl<T> Slot<T, SyncSignal> {
    pub fn wait_recv(&self, abort: &AtomicBool) -> Option<T> {
        loop {
            let item = self.0.lock().take();
            if let Some(item) = item {
                break Some(item);
            } else if abort.load(Ordering::SeqCst) {
                break None;
            } else {
                self.signal().wait()
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
                self.signal().wait_timeout(dur);
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

            self.signal().wait();
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
                self.signal().wait_timeout(dur);
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

struct Chan<T> {
    sending: Option<(usize, VecDeque<Arc<Slot<T, dyn signal::Signal>>>)>,
    queue: VecDeque<T>,
    waiting: VecDeque<Arc<Slot<T, dyn signal::Signal>>>,
}

impl<T> Chan<T> {
    fn pull_pending(&mut self, pull_extra: bool) {
        if let Some((cap, sending)) = &mut self.sending {
            let effective_cap = *cap + pull_extra as usize;

            while self.queue.len() < effective_cap {
                if let Some(s) = sending.pop_front() {
                    self.queue.push_back(s.fire_recv());
                } else {
                    break;
                }
            }
        }
    }
}

enum Block {
    None,
    Until(Instant),
    Forever,
}

struct Shared<T> {
    chan: Spinlock<Chan<T>>,
    disconnected: AtomicBool,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Shared<T> {
    fn new(cap: Option<usize>) -> Self {
        Self {
            chan: Spinlock::new(Chan {
                sending: cap.map(|cap| (cap, VecDeque::new())),
                queue: VecDeque::new(),
                waiting: VecDeque::new(),
            }),
            disconnected: AtomicBool::new(false),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),
        }
    }

    fn send<S: Signal, R: From<Result<(), TrySendTimeoutError<T>>>>(
        &self,
        item: T,
        should_block: bool,
        make_signal: impl FnOnce() -> S,
        do_block: impl FnOnce(Arc<Slot<T, S>>) -> R,
    ) -> R {
        let mut chan = wait_lock(&self.chan);

        if self.disconnected.load(Ordering::SeqCst) {
            Err(TrySendTimeoutError::Disconnected(item)).into()
        } else if let Some(r) = chan.waiting.pop_front() {
            debug_assert!(chan.queue.len() == 0);
            r.fire_send(item);
            Ok(()).into()
        } else if chan.sending.as_ref().map(|(cap, _)| chan.queue.len() < *cap).unwrap_or(true) {
            chan.queue.push_back(item);
            Ok(()).into()
        } else if should_block { // Only bounded from here on
            let slot = Slot::new(Some(item), make_signal());
            chan.sending.as_mut().unwrap().1.push_back(slot.clone());
            drop(chan);

            do_block(slot)
        } else {
            Err(TrySendTimeoutError::Full(item)).into()
        }
    }

    fn send_sync(
        &self,
        item: T,
        block: Option<Option<Instant>>,
    ) -> Result<(), TrySendTimeoutError<T>> {
        self.send(
            // item
            item,
            // should_block
            block.is_some(),
            // make_signal
            || SyncSignal::default(),
            // do_block
            |slot| if let Some(deadline) = block.unwrap() {
                slot.wait_deadline_send(&self.disconnected, deadline)
                    .or_else(|timed_out| {
                        if timed_out { // Remove our signal
                            let slot: Arc<Slot<T, dyn signal::Signal>> = slot.clone();
                            wait_lock(&self.chan).sending.as_mut().unwrap().1.retain(|s| !Arc::ptr_eq(s, &slot));
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
            },
        )
    }

    fn recv<S: Signal, R: From<Result<T, TryRecvTimeoutError>>>(
        &self,
        should_block: bool,
        make_signal: impl FnOnce() -> S,
        do_block: impl FnOnce(Arc<Slot<T, S>>) -> R,
    ) -> R {
        let mut chan = wait_lock(&self.chan);
        chan.pull_pending(true);

        if let Some(item) = chan.queue.pop_front() {
            Ok(item).into()
        } else if self.disconnected.load(Ordering::SeqCst) {
            Err(TryRecvTimeoutError::Disconnected).into()
        } else if should_block {
            let slot = Slot::new(None, make_signal());
            chan.waiting.push_back(slot.clone());
            drop(chan);

            do_block(slot)
        } else {
            Err(TryRecvTimeoutError::Empty).into()
        }
    }

    fn recv_sync(&self, block: Option<Option<Instant>>) -> Result<T, TryRecvTimeoutError> {
        self.recv(
            // should_block
            block.is_some(),
            // make_signal
            || SyncSignal::default(),
            // do_block
            |slot| if let Some(deadline) = block.unwrap() {
                slot.wait_deadline_recv(&self.disconnected, deadline)
                    .or_else(|timed_out| {
                        if timed_out { // Remove our signal
                            let slot: Arc<Slot<T, dyn Signal>> = slot.clone();
                            wait_lock(&self.chan).waiting.retain(|s| !Arc::ptr_eq(s, &slot));
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
            },
        )
    }

    /// Disconnect anything listening on this channel (this will not prevent receivers receiving
    /// items that have already been sent)
    fn disconnect_all(&self) {
        self.disconnected.store(true, Ordering::Relaxed);

        let mut chan = wait_lock(&self.chan);
        chan.pull_pending(false);
        chan.sending.as_ref().map(|(_, sending)| sending.iter().for_each(|slot| slot.signal().fire()));
        chan.waiting.iter().for_each(|slot| slot.signal().fire());
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn is_full(&self) -> bool {
        self.capacity().map(|cap| cap == self.len()).unwrap_or(false)
    }

    fn len(&self) -> usize {
        let mut chan = wait_lock(&self.chan);
        chan.pull_pending(false);
        chan.queue.len()
    }

    fn capacity(&self) -> Option<usize> {
        wait_lock(&self.chan).sending.as_ref().map(|(cap, _)| *cap)
    }
}

/// A transmitting end of a channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Attempt to send a value into the channel. If the channel is bounded and full, or the
    /// receiver has been dropped, an error is returned. If the channel associated with this
    /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.shared.send_sync(msg, None).map_err(|err| match err {
            TrySendTimeoutError::Full(msg) => TrySendError::Full(msg),
            TrySendTimeoutError::Disconnected(msg) => TrySendError::Disconnected(msg),
            _ => unreachable!(),
        })
    }

    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped. If the channel is bounded and is full, this method will block.
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.shared.send_sync(msg, Some(None)).map_err(|err| match err {
            TrySendTimeoutError::Disconnected(msg) => SendError(msg),
            _ => unreachable!(),
        })
    }

    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped or the deadline has passed. If the channel is bounded and is full, this method
    /// will block.
    pub fn send_deadline(&self, msg: T, deadline: Instant) -> Result<(), SendTimeoutError<T>> {
        self.shared.send_sync(msg, Some(Some(deadline))).map_err(|err| match err {
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
        self.shared.is_empty()
    }

    /// Returns true if the channel is full.
    /// Note: Zero-capacity channels are always full.
    pub fn is_full(&self) -> bool {
        self.shared.is_full()
    }

    /// Returns the number of messages in the channel
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// If the channel is bounded, returns its capacity.
    pub fn capacity(&self) -> Option<usize> {
        self.shared.capacity()
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
        self.shared.recv_sync(None).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => TryRecvError::Disconnected,
            TryRecvTimeoutError::Empty => TryRecvError::Empty,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv_sync(Some(None)).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => RecvError::Disconnected,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_sync(Some(Some(deadline))).map_err(|err| match err {
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
        let mut chan = wait_lock(&self.shared.chan);
        chan.pull_pending(false);
        let queue = std::mem::take(&mut chan.queue);

        Drain { queue, _phantom: PhantomData }
    }

    /// Returns true if the channel is empty.
    /// Note: Zero-capacity channels are always empty.
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty()
    }

    /// Returns true if the channel is full.
    /// Note: Zero-capacity channels are always full.
    pub fn is_full(&self) -> bool {
        self.shared.is_full()
    }

    /// Returns the number of messages in the channel.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// If the channel is bounded, returns its capacity.
    pub fn capacity(&self) -> Option<usize> {
        self.shared.capacity()
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
