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
    sync::{self, Arc, Condvar, Mutex, MutexGuard, WaitTimeoutResult, atomic::{AtomicUsize, AtomicBool, Ordering, spin_loop_hint}},
    time::{Duration, Instant},
    marker::PhantomData,
    thread,
    fmt,
};
#[cfg(windows)]
use std::sync::{Mutex as InnerMutex, MutexGuard as InnerMutexGuard};
#[cfg(not(windows))]
use spin::{Mutex as InnerMutex, MutexGuard as InnerMutexGuard};

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

#[derive(Default)]
pub struct Depot<T> {
    inner: InnerMutex<Vec<T>>,
}

const MAX_DEPOT_ITEMS: usize = 8;

impl<T> Depot<T> {
    #[inline(always)]
    pub fn with_one<R>(&self, make_fn: impl FnOnce() -> T, f: impl FnOnce(&mut T) -> R) -> R {
        let mut item = wait_lock(&self.inner).pop().unwrap_or_else(make_fn);
        let ret = f(&mut item);
        let mut inner = wait_lock(&self.inner);
        if inner.len() < MAX_DEPOT_ITEMS {
            inner.push(item);
        }
        ret
    }
}

#[derive(Default)]
struct Signal<T = ()> {
    lock: Mutex<T>,
    trigger: Condvar,
    waiters: AtomicUsize,
}

impl<T> Signal<T> {
    fn lock(&self) -> sync::MutexGuard<T> {
        wait_mutex(&self.lock)
    }

    fn wait<G>(&self, sync_guard: G) {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let _guard = self.trigger.wait(guard).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }

    fn wait_while<G>(&self, sync_guard: G, cond: impl FnMut(&mut T) -> bool) {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let _guard = self.trigger.wait_while(guard, cond).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }

    fn wait_timeout<G>(&self, sync_guard: G, dur: Duration) -> WaitTimeoutResult {
        self.wait_timeout_while(sync_guard, dur, |_| true)
    }

    fn wait_timeout_while<G>(&self, sync_guard: G, dur: Duration, cond: impl FnMut(&mut T) -> bool) -> WaitTimeoutResult {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let (_guard, timeout) = self.trigger.wait_timeout_while(guard, dur, cond).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
        timeout
    }

    fn notify_one<G>(&self, sync_guard: G) {
        self.notify_one_with(sync_guard, |_| ());
    }

    fn notify_one_with<G>(&self, sync_guard: G, f: impl FnOnce(&mut T)) {
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
fn wait_mutex<'a, T>(lock: &'a Mutex<T>) -> MutexGuard<'a, T> {
    let mut i = 0;
    loop {
        for _ in 0..6 {
            if let Ok(guard) = lock.try_lock() {
                return guard;
            }
            thread::yield_now();
        }
        i += 1;
        thread::sleep(Duration::from_nanos(i * 50));
    }
}

#[inline]
#[cfg(not(windows))]
fn wait_lock<'a, T>(lock: &'a InnerMutex<T>) -> InnerMutexGuard<'a, T> {
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
#[cfg(windows)]
fn wait_lock<'a, T>(lock: &'a InnerMutex<T>) -> InnerMutexGuard<'a, T> {
    lock.lock().unwrap()
}

#[inline]
#[cfg(feature = "async")]
fn poll_lock<'a, T>(lock: &'a InnerMutex<T>) -> Option<InnerMutexGuard<'a, T>> {
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
        pending: InnerMutex<BoundedQueues<T>>,
    },
    Unbounded {
        send_signal: Signal<()>,
        queue: InnerMutex<VecDeque<T>>,
    },
}

struct Shared<T> {
    chan: Channel<T>,
    disconnected: AtomicBool,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
    sig_depot: Depot<Arc<Signal<Option<T>>>>,
}

impl<T> Shared<T> {
    fn new(cap: Option<usize>) -> Self {
        Self {
            chan: match cap {
                Some(cap) =>  Channel::Bounded {
                    cap,
                    pending: InnerMutex::new(BoundedQueues {
                        senders: VecDeque::new(),
                        queue: VecDeque::with_capacity(cap),
                        receivers: VecDeque::new(),
                    }),
                },
                None => Channel::Unbounded {
                    queue: InnerMutex::new(VecDeque::new()),
                    send_signal: Signal::default(),
                },
            },
            disconnected: AtomicBool::new(false),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),
            sig_depot: Depot::default(),
        }
    }

    /// Disconnect anything listening on this channel (this will not prevent receivers receiving
    /// items that have already been sent)
    fn disconnect_all(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
        match &self.chan {
            Channel::Bounded { pending, .. } => {
                let pending = wait_lock(&pending);
                pending.senders.iter().for_each(|s| s.notify_all(()));
                pending.receivers.iter().for_each(|s| s.notify_all(()));
            },
            Channel::Unbounded { queue, send_signal } => {
                let queue = wait_lock(&queue);
                send_signal.notify_all(queue);
            },
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
            Channel::Bounded { pending, cap } => {
                let mut pending = wait_lock(&pending);
                pull_pending(*cap, &mut pending);
                pending.queue.len()
            },
            Channel::Unbounded { queue, .. } => {
                wait_lock(&queue).len()
            },
        }
    }

    fn capacity_inner(&self) -> Option<usize> {
        match &self.chan {
            Channel::Bounded { cap, .. } => Some(*cap),
            Channel::Unbounded { .. } => None,
        }
    }
}

/// A transmitting end of a channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Sender<T> {
    #[inline(always)]
    fn send_inner(&self, msg: T, block: Option<Option<Instant>>) -> Result<(), TrySendTimeoutError<T>> {
        match &self.shared.chan {
            Channel::Bounded { cap, pending: pending_lock } => {
                let mut pending = wait_lock(&pending_lock);
                if self.shared.disconnected.load(Ordering::Relaxed) {
                    return Err(TrySendTimeoutError::Disconnected(msg));
                } else if let Some(recv) = pending.receivers.pop_front() {
                    debug_assert!(pending.queue.len() == 0);
                    recv.notify_one_with((), |m| *m = Some(msg));
                    Ok(())
                } else if pending.queue.len() < *cap {
                    pending.queue.push_back(msg);
                    Ok(())
                } else if let Some(deadline) = block {
                    self.shared.sig_depot.with_one(
                        || Arc::new(Signal::default()),
                        move |sig| {
                            *sig.lock() = Some(msg);
                            pending.senders.push_back(sig.clone());

                            if let Some(deadline) = deadline {
                                let now = Instant::now();
                                let timeout = sig.wait_timeout_while(
                                    pending,
                                    deadline.saturating_duration_since(now),
                                    |msg| msg.is_some() && !self.shared.disconnected.load(Ordering::Relaxed),
                                );

                                if let Some(msg) = sig.lock().take() {
                                    if timeout.timed_out() {
                                        // Remove our signal
                                        wait_lock(&pending_lock).senders.retain(|slot| !Arc::ptr_eq(slot, sig));

                                        Err(TrySendTimeoutError::Timeout(msg))
                                    } else {
                                        Err(TrySendTimeoutError::Disconnected(msg))
                                    }
                                } else {
                                    Ok(())
                                }
                            } else {
                                sig.wait_while(pending, |msg| {
                                    msg.is_some() && !self.shared.disconnected.load(Ordering::Relaxed)
                                });

                                if let Some(msg) = sig.lock().take() {
                                    Err(TrySendTimeoutError::Disconnected(msg))
                                } else {
                                    Ok(())
                                }
                            }
                        },
                    )
                } else {
                    Err(TrySendTimeoutError::Full(msg))
                }
            },
            Channel::Unbounded { queue, send_signal } => {
                if self.shared.disconnected.load(Ordering::Relaxed) {
                    Err(TrySendTimeoutError::Disconnected(msg))
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
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.send_inner(msg, Some(None)).map_err(|err| match err {
            TrySendTimeoutError::Disconnected(msg) => SendError(msg),
            _ => unreachable!(),
        })
    }

    /// Attempt to send a value into the channel. If the channel is bounded and full, or the
    /// receiver has been dropped, an error is returned. If the channel associated with this
    /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.send_inner(msg, None).map_err(|err| match err {
            TrySendTimeoutError::Full(msg) => TrySendError::Full(msg),
            TrySendTimeoutError::Disconnected(msg) => TrySendError::Disconnected(msg),
            _ => unreachable!(),
        })
    }

    /// Send a value into the channel, returning an error if the channel receiver has
    /// been dropped or the deadline has passed. If the channel is bounded and is full, this method
    /// will block.
    pub fn send_deadline(&self, msg: T, deadline: Instant) -> Result<(), SendTimeoutError<T>> {
        self.send_inner(msg, Some(Some(deadline))).map_err(|err| match err {
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

fn pull_pending<T>(
    effective_cap: usize,
    pending: &mut BoundedQueues<T>,
) {
    while pending.queue.len() < effective_cap {
        if let Some(signal) = pending.senders.pop_front() {
            let mut msg = None;
            signal.notify_one_with((), |m| msg = m.take());
            if let Some(msg) = msg {
                pending.queue.push_back(msg);
            }
        } else {
            break;
        }
    }
}

impl<T> Receiver<T> {
    #[inline(always)]
    fn recv_inner(&self, block: Option<Option<Instant>>) -> Result<T, TryRecvTimeoutError> {
        match &self.shared.chan {
            Channel::Bounded { cap, pending: pending_lock } => {
                let mut pending = wait_lock(&pending_lock);
                pull_pending(*cap + 1, &mut pending);
                if let Some(msg) = pending.queue.pop_front() {
                    Ok(msg)
                } else if self.shared.disconnected.load(Ordering::Relaxed) {
                    return Err(TryRecvTimeoutError::Disconnected);
                } else if let Some(deadline) = block {
                    self.shared.sig_depot.with_one(
                        || Arc::new(Signal::default()),
                        move |sig| {
                            pending.receivers.push_back(sig.clone());

                            if let Some(deadline) = deadline {
                                let now = Instant::now();
                                if sig.wait_timeout_while(
                                    pending,
                                    deadline.saturating_duration_since(now),
                                    |msg| msg.is_none() && !self.shared.disconnected.load(Ordering::Relaxed),
                                ).timed_out() {
                                    // There is a problem here. When calling `rx.recv()`, a signal
                                    // gets inserted into the receiver queue to be matched with
                                    // a sent item later, causing it to unblock. However, calling
                                    // `rx.recv_timeout()` may exit early, leaving a "phoney"
                                    // signal in the receiver queue that is no longer valid. To
                                    // resolve this, we do something a little hacky: run through
                                    // the receiver queue, find our signal, and remove it. This is
                                    // expensive, but thankfully it's an edge-case that hopefully
                                    // won't have an impact on performance-critical code (after
                                    // all, this only happens when a timeout has occurred).
                                    let mut pending = wait_lock(&pending_lock);
                                    // Remove our signal
                                    pending.receivers.retain(|slot| !Arc::ptr_eq(slot, sig));
                                    // Check one last time for a value - perhaps one got added in
                                    // the meantime?
                                    return match sig.lock().take() {
                                        Some(msg) => Ok(msg),
                                        None => Err(TryRecvTimeoutError::Timeout),
                                    };
                                }
                            } else {
                                sig.wait_while(pending, |msg| {
                                    msg.is_none() && !self.shared.disconnected.load(Ordering::Relaxed)
                                });
                            }

                            sig
                                .lock()
                                .take()
                                .ok_or(TryRecvTimeoutError::Disconnected)
                        },
                    )
                } else {
                    Err(TryRecvTimeoutError::Empty)
                }
            },
            Channel::Unbounded { send_signal, queue } => loop {
                let mut queue = wait_lock(&queue);
                if let Some(msg) = queue.pop_front() {
                    break Ok(msg);
                } else if self.shared.disconnected.load(Ordering::Relaxed) {
                    break Err(TryRecvTimeoutError::Disconnected);
                } else if let Some(deadline) = block {
                    if let Some(deadline) = deadline {
                        let now = Instant::now();
                        if send_signal
                            .wait_timeout(queue, deadline.saturating_duration_since(now))
                            .timed_out()
                        {
                            break Err(TryRecvTimeoutError::Timeout);
                        }
                    } else {
                        send_signal.wait(queue);
                    }
                } else {
                    break Err(TryRecvTimeoutError::Empty);
                }
            },
        }
    }


    /// Attempt to fetch an incoming value from the channel associated with this receiver,
    /// returning an error if the channel is empty or all channel senders have been dropped.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.recv_inner(None).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => TryRecvError::Disconnected,
            TryRecvTimeoutError::Empty => TryRecvError::Empty,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.recv_inner(Some(None)).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => RecvError::Disconnected,
            _ => unreachable!(),
        })
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.recv_inner(Some(Some(deadline))).map_err(|err| match err {
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
