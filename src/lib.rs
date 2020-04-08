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

// Reexports
#[cfg(feature = "select")]
pub use select::Selector;

use std::{
    collections::VecDeque,
    sync::{self, Arc, Condvar, Mutex, WaitTimeoutResult, atomic::{AtomicUsize, AtomicBool, Ordering}},
    time::{Duration, Instant},
    cell::UnsafeCell,
    marker::PhantomData,
    thread,
    fmt,
};
#[cfg(windows)]
use std::sync::{Mutex as InnerMutex, MutexGuard};
#[cfg(not(windows))]
use spin::{Mutex as InnerMutex, MutexGuard};

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

/// An error that may be emitted when attempting to wait for a value on a receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
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
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
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
            TryRecvError::Disconnected => "receiving on a closed channel".fmt(f),
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
            RecvTimeoutError::Timeout => "timed out waiting on channel".fmt(f),
            RecvTimeoutError::Disconnected => "channel is empty and sending half is closed".fmt(f),
        }
    }
}

impl std::error::Error for RecvTimeoutError {}

#[derive(Default)]
struct Signal<T = ()> {
    lock: Mutex<T>,
    trigger: Condvar,
    waiters: AtomicUsize,
}

impl<T> Signal<T> {
    fn lock(&self) -> sync::MutexGuard<T> {
        self.lock.lock().unwrap()
    }

    fn wait<G>(&self, sync_guard: G) {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let _guard = self.trigger.wait(guard).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }

    fn wait_timeout<G>(&self, dur: Duration, sync_guard: G) -> WaitTimeoutResult {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let (_guard, timeout) = self.trigger.wait_timeout(guard, dur).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
        timeout
    }

    fn wait_while<G>(&self, sync_guard: G, cond: impl FnMut(&mut T) -> bool) {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let _guard = self.trigger.wait_while(guard, cond).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }

    fn notify_one<G>(&self, sync_guard: G) {
        if self.waiters.load(Ordering::Relaxed) > 0 {
            drop(sync_guard);
            let _guard = self.lock.lock().unwrap();
            self.trigger.notify_one();
        }
    }

    fn notify_one_with<G>(&self, f: impl FnOnce(&mut T), sync_guard: G) {
        if self.waiters.load(Ordering::Relaxed) > 0 {
            drop(sync_guard);
            let mut guard = self.lock.lock().unwrap();
            f(&mut *guard);
            self.trigger.notify_one();
        }
    }

    fn notify_all<G>(&self, sync_guard: G) {
        if self.waiters.load(Ordering::Relaxed) > 0 {
            drop(sync_guard);
            let _guard = self.lock.lock().unwrap();
            self.trigger.notify_all();
        }
    }
}

#[inline]
#[cfg(not(windows))]
fn wait_lock<'a, T>(lock: &'a InnerMutex<T>) -> MutexGuard<'a, T> {
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

#[inline]
fn poll_lock<'a, T>(lock: &'a InnerMutex<T>) -> Option<MutexGuard<'a, T>> {
    lock.try_lock()
}

/// Invariants; While locked, one of the following will be true:
/// - The queue will be full (i.e: contain example `cap` items)
/// - the `senders` queue will be empty
/// Additionally, a non-empty queue implies that `receivers` will be empty
struct BoundedQueues<T> {
    senders: VecDeque<Arc<Signal<Option<T>>>>,
    queue: VecDeque<T>,
    receivers: VecDeque<Arc<Signal<Option<T>>>>,
}

enum Channel<T> {
    Bounded {
        cap: usize,
        pending: spin::Mutex<BoundedQueues<T>>,
    },
    Unbounded {
        queue: spin::Mutex<VecDeque<T>>,
        send_signal: Signal<()>,
    },
}

struct Shared<T> {
    chan: Channel<T>,
    disconnected: AtomicBool,
    sender_count: AtomicUsize,
}

impl<T> Shared<T> {
    fn new(cap: Option<usize>) -> Self {
        Self {
            chan: if let Some(cap) = cap {
                Channel::Bounded {
                    cap,
                    pending: spin::Mutex::new(BoundedQueues {
                        senders: VecDeque::new(),
                        queue: VecDeque::new(),
                        receivers: VecDeque::new(),
                    }),
                }
            } else {
                Channel::Unbounded {
                    queue: spin::Mutex::new(VecDeque::new()),
                    send_signal: Signal::default(),
                }
            },
            disconnected: AtomicBool::new(false),
            sender_count: AtomicUsize::new(1),
        }
    }

    /// Inform the receiver that all senders have been dropped
    #[inline]
    fn all_senders_disconnected(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
        match &self.chan {
            Channel::Bounded { pending, .. } => {
                let pending = pending.lock();
                pending.senders.iter().for_each(|s| s.notify_all(()));
                pending.receivers.iter().for_each(|s| s.notify_all(()));
            },
            Channel::Unbounded { queue, send_signal } => {
                let queue = queue.lock();
                send_signal.notify_all(queue);
            },
        }
    }

    #[inline]
    fn receiver_disconnected(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
        match &self.chan {
            Channel::Bounded { pending, .. } => {
                let pending = pending.lock();
                pending.senders.iter().for_each(|s| s.notify_all(()));
                pending.receivers.iter().for_each(|s| s.notify_all(()));
            },
            Channel::Unbounded { queue, send_signal } => {
                let queue = queue.lock();
                send_signal.notify_all(queue);
            },
        }
    }
}

/// A transmitting end of a channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
    unblock_signal: Option<Arc<Signal<Option<T>>>>,
    /// Used to prevent Sync being implemented for this type - we never actually use it!
    /// TODO: impl<T> !Sync for Receiver<T> {} when negative traits are stable
    _phantom_cell: UnsafeCell<()>,
}

impl<T> Sender<T> {
    fn send_inner(&self, msg: T, block: bool) -> Result<(), TrySendError<T>> {
        match &self.shared.chan {
            Channel::Bounded { cap, pending } => {
                let mut pending = wait_lock(&pending);
                if self.shared.disconnected.load(Ordering::Relaxed) {
                    return Err(TrySendError::Disconnected(msg));
                } else if let Some(recv) = pending.receivers.pop_front() {
                    debug_assert!(pending.queue.len() == 0);
                    recv.notify_one_with(|m| *m = Some(msg), ());
                    Ok(())
                } else if pending.queue.len() < *cap {
                    pending.queue.push_back(msg);
                    Ok(())
                } else if block {
                    let unblock_signal = self.unblock_signal.as_ref().unwrap();
                    *unblock_signal.lock() = Some(msg);
                    pending.senders.push_back(unblock_signal.clone());

                    unblock_signal.wait_while(pending, |msg| {
                        msg.is_some() && !self.shared.disconnected.load(Ordering::Relaxed)
                    });

                    if let Some(msg) = unblock_signal.lock().take() {
                        Err(TrySendError::Disconnected(msg))
                    } else {
                        Ok(())
                    }
                } else {
                    Err(TrySendError::Full(msg))
                }
            },
            Channel::Unbounded { queue, send_signal } => {
                if self.shared.disconnected.load(Ordering::Relaxed) {
                    Err(TrySendError::Disconnected(msg))
                } else {
                    let mut queue = wait_lock(&queue);
                    queue.push_back(msg);
                    send_signal.notify_one(queue);
                    Ok(())
                }
            },
        }
    }

    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped. If the channel is bounded and is full, this method will block.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.send_inner(msg, false)
    }

    /// Attempt to send a value into the channel. If the channel is bounded and full, or the
    /// receiver has been dropped, an error is returned. If the channel associated with this
    /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.send_inner(msg, true).map_err(|err| match err {
            TrySendError::Disconnected(msg) => SendError(msg),
            TrySendError::Full(_) => unreachable!(),
        })
    }

    // /// Send a value into the channel, returning an error if the channel receiver has
    // /// been dropped. If the channel is bounded and is full, this method will block.
    // pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
    //     self.shared.send(msg)
    // }

    // /// Attempt to send a value into the channel. If the channel is bounded and full, or the
    // /// receiver has been dropped, an error is returned. If the channel associated with this
    // /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
    // pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
    //     self.shared.try_send(msg).map(|_| ()).map_err(|(_, err)| err)
    // }
}

impl<T> Clone for Sender<T> {
    /// Clone this sender. [`Sender`] acts as a handle to a channel, and the channel will only be
    /// cleaned up when all senders and the receiver have been dropped.
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
            unblock_signal: Some(Arc::new(Signal::default())),
            _phantom_cell: UnsafeCell::new(()),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Notify the receiver that all senders have been dropped if the number of senders drops
        // to 0.
        if self.shared.sender_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.all_senders_disconnected();
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

/// The receiving end of a channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    unblock_signal: Option<Arc<Signal<Option<T>>>>,
    /// Used to prevent Sync being implemented for this type - we never actually use it!
    /// TODO: impl<T> !Sync for Receiver<T> {} when negative traits are stable
    _phantom_cell: UnsafeCell<()>,
}

fn pull_pending<T>(
    effective_cap: usize,
    pending: &mut BoundedQueues<T>,
) {
    while pending.queue.len() < effective_cap {
        if let Some(signal) = pending.senders.pop_front() {
            let mut msg = None;
            signal.notify_one_with(|m| msg = m.take(), ());
            if let Some(msg) = msg {
                pending.queue.push_back(msg);
            }
        } else {
            break;
        }
    }
}

impl<T> Receiver<T> {
    fn recv_inner(&self, block: bool) -> Result<T, TryRecvError> {
        match &self.shared.chan {
            Channel::Bounded { cap, pending } => {
                let mut pending = wait_lock(&pending);
                pull_pending(*cap + 1, &mut pending);
                if let Some(msg) = pending.queue.pop_front() {
                    Ok(msg)
                } else if self.shared.disconnected.load(Ordering::Relaxed) {
                    return Err(TryRecvError::Disconnected);
                } else if block {
                    let unblock_signal = self.unblock_signal.as_ref().unwrap();
                    pending.receivers.push_back(unblock_signal.clone());

                    unblock_signal.wait_while(pending, |msg| {
                        msg.is_none() && !self.shared.disconnected.load(Ordering::Relaxed)
                    });

                    unblock_signal
                        .lock()
                        .take()
                        .ok_or(TryRecvError::Disconnected)
                } else {
                    Err(TryRecvError::Empty)
                }
            },
            Channel::Unbounded { send_signal, queue } => loop {
                let mut queue = wait_lock(&queue);
                if let Some(msg) = queue.pop_front() {
                    break Ok(msg);
                } else if self.shared.disconnected.load(Ordering::Relaxed) {
                    break Err(TryRecvError::Disconnected);
                } else if block {
                    send_signal.wait(queue);
                } else {
                    break Err(TryRecvError::Empty);
                }
            },
        }
    }


    /// Attempt to fetch an incoming value from the channel associated with this receiver,
    /// returning an error if the channel is empty or all channel senders have been dropped.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.recv_inner(false)
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.recv_inner(true).map_err(|err| match err {
            TryRecvError::Disconnected => RecvError::Disconnected,
            TryRecvError::Empty => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the timeout has expired.
    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_deadline(Instant::now().checked_add(dur).unwrap())
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        match &self.shared.chan {
            Channel::Bounded { cap, pending } => loop {

                todo!("recv_deadline for bounded")
                /*
                let (mut queue, mut pending) = (wait_lock(&queue), wait_lock(&pending));
                pull_pending(*cap, &mut pending, &mut queue);
                if let Some(msg) = queue.pop_front() {
                    break Ok(msg);
                } else if self.shared.disconnected.load(Ordering::Relaxed) {
                    break Err(RecvTimeoutError::Disconnected);
                } else {
                    let now = Instant::now();
                    if self
                        .shared
                        .send_signal
                        .wait_timeout(deadline.duration_since(now), (queue, pending))
                        .timed_out()
                    {
                        break Err(RecvTimeoutError::Timeout);
                    }
                }
                */
            },
            Channel::Unbounded { queue, send_signal } => loop {
                let mut queue = wait_lock(&queue);
                if let Some(msg) = queue.pop_front() {
                    break Ok(msg);
                } else if self.shared.disconnected.load(Ordering::Relaxed) {
                    break Err(RecvTimeoutError::Disconnected);
                } else {
                    let now = Instant::now();
                    if send_signal
                        .wait_timeout(deadline.duration_since(now), queue)
                        .timed_out()
                    {
                        break Err(RecvTimeoutError::Timeout);
                    }
                }
            },
        }
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
            Channel::Bounded { cap, pending } => {
                let mut pending = wait_lock(&pending);
                let msgs = std::mem::take(&mut pending.queue);
                pull_pending(*cap, &mut pending);
                msgs
            },
            Channel::Unbounded { queue, .. } => std::mem::take(&mut *wait_lock(&queue)),
        };

        Drain { queue, _phantom: PhantomData }
    }
}

impl<T> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { receiver: self }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.receiver_disconnected();
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
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
        Sender {
            shared: shared.clone(),
            unblock_signal: None,
            _phantom_cell: UnsafeCell::new(())
        },
        Receiver {
            shared,
            unblock_signal: Some(Arc::new(Signal::default())),
            _phantom_cell: UnsafeCell::new(())
        },
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
        Sender {
            shared: shared.clone(),
            unblock_signal: Some(Arc::new(Signal::default())),
            _phantom_cell: UnsafeCell::new(())
        },
        Receiver {
            shared,
            unblock_signal: Some(Arc::new(Signal::default())),
            _phantom_cell: UnsafeCell::new(())
        },
    )
}
