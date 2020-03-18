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

// Reexports
#[cfg(feature = "select")]
pub use select::Selector;

use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::{Duration, Instant},
    cell::UnsafeCell,
    thread,
};
use std::sync::{Condvar, Mutex};

/// An error that may be emitted when attempting to send a value into a channel on a sender.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

/// An error that may be emitted when attempting to wait for a value on a receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

/// An error that may be emitted when attempting to fetch a value on a receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

/// An error that may be emitted when attempting to wait for a value on a receiver with a timeout.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

/// Wrapper around a queue. This wrapper exists to permit a maximum length.
struct Queue<T>(VecDeque<T>, Option<usize>);

impl<T> Queue<T> {
    fn new() -> Self { Self(VecDeque::new(), None) }
    fn bounded(n: usize) -> Self { Self(VecDeque::new(), Some(n)) }

    fn push(&mut self, x: T) -> Option<T> {
        if Some(self.0.len()) == self.1 {
            Some(x)
        } else {
            self.0.push_back(x);
            None
        }
    }

    fn pop(&mut self) -> Option<T> {
        self.0.pop_front()
    }
}

struct Shared<T> {
    queue: spin::Mutex<Queue<T>>,
    /// Mutexed used for locking condvars.
    wait_lock: Mutex<()>,
    // Used for notifying the receiver about incoming messages.
    send_trigger: Condvar,
    send_selectors: spin::Mutex<(usize, Vec<(usize, Arc<SelectorSignal>, Token)>)>,
    recv_selector: spin::Mutex<Option<(Arc<SelectorSignal>, Token)>>,
    // Used for notifying senders about the queue no longer being full. Therefore, this is only a
    // `Some` for bounded queues.
    recv_trigger: Option<Condvar>,
    /// The number of senders associated with this channel. If this drops to 0, the channel is
    /// 'dead' and the listener will begin reporting disconnect errors (once the queue has been
    /// drained).
    senders: AtomicUsize,
    /// The number of senders waiting for notifications that the queue has space.
    send_waiters: AtomicUsize,
    /// An atomic used to describe the state of the receiving end of the queue:
    /// - 0 => Receiver has been dropped, so the channel is 'dead'
    /// - 1 => Receiver still exists, but is not waiting for notifications
    /// - x => Receiver is waiting for incoming message notifications
    listen_mode: AtomicUsize,
}

impl<T> Shared<T> {
    fn try_send(&self, msg: T) -> Result<(), (spin::MutexGuard<Queue<T>>, TrySendError<T>)> {
        loop {
            // Attempt to lock the queue. Upon success, attempt to receive. If the queue is empty,
            // we don't block anyway so just break out of the loop.
            if let Some(mut queue) = self.queue.try_lock() {
                let listen_mode = self.listen_mode.load(Ordering::Relaxed);
                if listen_mode == 0 { // If the listener has disconnected, the channel is dead
                    break Err((queue, TrySendError::Disconnected(msg)));
                } else {
                    // If pushing fails, it's because the queue is full
                    if let Some(msg) = queue.push(msg) {
                        break Err((queue, TrySendError::Full(msg)));
                    } else if let Some((signal, token)) = &*self.recv_selector.lock() {
                        // If there is a recv selector attached, notify that only.
                        let mut guard = signal.wait_lock.lock().unwrap();
                        drop(queue);
                        *guard = Some(*token);
                        signal.trigger.notify_one();
                    } else if listen_mode > 1 {
                        // Notify the receiver of a new message if listeners are waiting
                        let _ = self.wait_lock.lock().unwrap();
                        // Drop the queue early to avoid a deadlock
                        drop(queue);
                        self.send_trigger.notify_one();
                    } else {
                        drop(queue);
                    }
                    break Ok(());
                }
            } else {
                // If we can't gain access to the queue, yield until the next time slice
                thread::yield_now();
            }
        }
    }

    fn send(&self, mut msg: T) -> Result<(), SendError<T>> {
        loop {
            // Attempt to send a message
            let queue = match self.try_send(msg) {
                Ok(()) => break Ok(()),
                Err((_, TrySendError::Disconnected(msg))) => break Err(SendError(msg)),
                Err((queue, TrySendError::Full(m))) => {
                    msg = m;
                    queue
                },
            };

            if let Some(recv_trigger) = self.recv_trigger.as_ref() {
                // Take a guard of the main lock to use later when waiting
                let guard = self.wait_lock.lock().unwrap();
                // Inform the receiver that we need waking
                self.send_waiters.fetch_add(1, Ordering::Acquire);
                // We keep the queue alive until here to avoid a deadlock
                drop(queue);
                // Wait until we get a signal that suggests the queue might have space
                let _ = recv_trigger.wait(guard).unwrap();
                // Inform the receiver that we no longer need waking
                self.send_waiters.fetch_sub(1, Ordering::Release);
            }
        }
    }

    /// Inform the receiver that all senders have been dropped
    fn all_senders_disconnected(&self) {
        let _ = self.wait_lock.lock().unwrap();
        self.send_trigger.notify_all(); // TODO: notify_one instead? Which is faster?
    }

    fn receiver_disconnected(&self) {
        if let Some(recv_trigger) = self.recv_trigger.as_ref() {
            let _ = self.wait_lock.lock().unwrap();
            recv_trigger.notify_all();
        }
    }

    fn try_recv(&self) -> Result<T, (spin::MutexGuard<Queue<T>>, TryRecvError)> {
        loop {
            // Attempt to lock the queue. Upon success, attempt to receive. If the queue is empty,
            // we don't block anyway so just break out of the loop.
            if let Some(mut queue) = self.queue.try_lock() {
                break if let Some(msg) = queue.pop() {
                    // If there are senders waiting for a message, wake them up.
                    if let Some(recv_trigger) = self.recv_trigger.as_ref() {
                        if queue.1.is_some() && self.send_waiters.load(Ordering::Relaxed) > 0 {
                            let _ = self.wait_lock.lock().unwrap();
                            drop(queue);
                            recv_trigger.notify_one();
                        }
                    }
                    // Notify send selectors
                    self
                        .send_selectors
                        .lock()
                        .1
                        .iter()
                        .for_each(|(_, signal, token)| {
                            let mut guard = signal.wait_lock.lock().unwrap();
                            *guard = Some(*token);
                            signal.trigger.notify_one();
                        });
                    Ok(msg)
                } else if self.senders.load(Ordering::Relaxed) == 0 {
                    // If there's nothing more in the queue, this might be because there are no
                    // more senders.
                    Err((queue, TryRecvError::Disconnected))
                } else {
                    Err((queue, TryRecvError::Empty))
                };
            } else {
                // If we can't gain access to the queue, yield until the next time slice
                thread::yield_now();
            }
        }
    }

    fn recv(&self) -> Result<T, RecvError> {
        loop {
            // Attempt to receive a message
            let queue = match self.try_recv() {
                Ok(msg) => break Ok(msg),
                Err((_, TryRecvError::Disconnected)) => break Err(RecvError::Disconnected),
                Err((queue, TryRecvError::Empty)) => queue,
            };

            // Take a guard of the main lock to use later when waiting
            let guard = self.wait_lock.lock().unwrap();
            // Inform the receiver that we need waking
            self.listen_mode.fetch_add(1, Ordering::Acquire);
            // We keep the queue alive until here to avoid a deadlock
            drop(queue);
            // Wait until we get a signal that the queue has new messages
            let _ = self.send_trigger.wait(guard).unwrap();
            // Inform the receiver that we no longer need waking
            self.listen_mode.fetch_sub(1, Ordering::Release);
        }
    }

    // TODO: Change this to `recv_timeout` to potentially avoid an extra call to `Instant::now()`?
    fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        // Attempt a speculative recv. If we are lucky there might be a message in the queue!
        if let Ok(msg) = self.try_recv() {
            return Ok(msg);
        }

        let mut guard = self.wait_lock.lock().unwrap();
        // Inform senders that we're going into a listening period and need to be notified of new
        // messages.
        self.listen_mode.store(2, Ordering::Relaxed);
        let result = loop {
            // TODO: Instant::now() is expensive, find a better way to do this
            let now = Instant::now();
            let timeout = if now >= deadline {
                // We've hit the deadline and found nothing, produce a timeout error.
                break Err(RecvTimeoutError::Timeout);
            } else {
                // Calculate the new timeout
                deadline.duration_since(now)
            };

            // Wait for the given timeout (or, at least, try to - this may complete before the
            // timeout due to spurious wakeup events).
            let (nguard, timeout) = self.send_trigger.wait_timeout(guard, timeout).unwrap();
            guard = nguard;
            if timeout.timed_out() {
                // This was a timeout rather than a wakeup, so produce a timeout error.
                break Err(RecvTimeoutError::Timeout);
            }

            // Attempt to receive a message from the queue
            match self.try_recv() {
                Ok(msg) => break Ok(msg),
                Err((_, TryRecvError::Empty)) => {},
                Err((_, TryRecvError::Disconnected)) => break Err(RecvTimeoutError::Disconnected),
            }
        };
        // Ensure the listen mode is reset
        self.listen_mode.store(1, Ordering::Relaxed);
        result
    }

    fn connect_send_selector(&self, signal: Arc<SelectorSignal>, token: Token) -> usize {
        let (id, signals) = &mut *self.send_selectors.lock();
        *id += 1;
        signals.push((*id, signal, token));
        *id
    }

    fn disconnect_send_selector(&self, id: usize) {
        self.send_selectors.lock().1.retain(|(s_id, _, _)| s_id != &id);
    }

    fn connect_recv_selector(&self, signal: Arc<SelectorSignal>, token: Token) {
        *self.recv_selector.lock() = Some((signal, token))
    }

    fn disconnect_recv_selector(&self) {
        *self.recv_selector.lock() = None;
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
        self.shared.send(msg)
    }

    /// Attempt to send a value into the channel. If the channel is bounded and full, or the
    /// receiver has been dropped, an error is returned. If the channel associated with this
    /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.shared.try_send(msg).map_err(|(_, err)| err)
    }
}

impl<T> Clone for Sender<T> {
    /// Clone this sender. [`Sender`] acts as a handle to a channel, and the channel will only be
    /// cleaned up when all senders and the receiver have been dropped.
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Notify the receiver that all senders have been dropped if the number of senders drops
        // to 0. Note that `fetch_add` returns the old value, so we test for 1.
        if self.shared.senders.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.all_senders_disconnected();
        }
    }
}

/// The receiving end of a channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    /// Used to prevent Sync being implemented for this type - we never actually use it!
    /// TODO: impl<T> !Sync for Receiver<T> {} when negative traits are stable
    _phantom_cell: UnsafeCell<()>,
}

impl<T> Receiver<T> {
    /// Wait for an incoming value on this receiver, returning an error if all channel senders have
    /// been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv()
    }

    /// Wait for an incoming value on this receiver, returning an error if all channel senders have
    /// been dropped or the timeout has expired.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(Instant::now().checked_add(timeout).unwrap())
    }

    /// Wait for an incoming value on this receiver, returning an error if all channel senders have
    /// been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(deadline)
    }

    /// Attempt to fetch an incoming value on this receiver, returning an error if the channel is
    /// empty or all channel senders have been dropped.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.shared.try_recv().map_err(|(_, err)| err)
    }

    /// A blocking iterator over the values received on the channel that finishes iteration when
    /// all receivers of the channel have been dropped.
    pub fn iter(&self) -> impl Iterator<Item=T> + '_ {
        Iter { receiver: &self }
    }

    /// A non-blocking iterator over the values received on the channel that finishes iteration
    /// when all receivers of the channel have been dropped or the channel is empty.
    pub fn try_iter(&self) -> impl Iterator<Item=T> + '_ {
        TryIter { receiver: &self }
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
        self.shared.listen_mode.store(0, Ordering::Relaxed);
        self.shared.receiver_disconnected();

        // Ensure that, as intended, the listen_mode has fallen back to 0 when we're done.
        // TODO: Remove this when we're 100% certain that this works fine.
        debug_assert!(self.shared.listen_mode.load(Ordering::Relaxed) == 0);
    }
}

/// An iterator over the items received from a channel.
pub struct Iter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.recv().ok()
    }
}

/// An non-blocking iterator over the items received from a channel.
pub struct TryIter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.try_recv().ok()
    }
}

/// An owned iterator over the items received from a channel.
pub struct IntoIter<T> {
    receiver: Receiver<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.recv().ok()
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
    let shared = Arc::new(Shared {
        queue: spin::Mutex::new(Queue::new()),
        wait_lock: Mutex::new(()),
        send_trigger: Condvar::new(),
        send_selectors: spin::Mutex::new((0, Vec::new())),
        recv_selector: spin::Mutex::new(None),
        recv_trigger: None,
        senders: AtomicUsize::new(1),
        send_waiters: AtomicUsize::new(0),
        listen_mode: AtomicUsize::new(1),
    });
    (
        Sender { shared: shared.clone() },
        Receiver { shared, _phantom_cell: UnsafeCell::new(()) },
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
pub fn bounded<T>(n: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        queue: spin::Mutex::new(Queue::bounded(n)),
        wait_lock: Mutex::new(()),
        send_trigger: Condvar::new(),
        send_selectors: spin::Mutex::new((0, Vec::new())),
        recv_selector: spin::Mutex::new(None),
        recv_trigger: Some(Condvar::new()),
        senders: AtomicUsize::new(1),
        send_waiters: AtomicUsize::new(0),
        listen_mode: AtomicUsize::new(1),
    });
    (
        Sender { shared: shared.clone() },
        Receiver { shared, _phantom_cell: UnsafeCell::new(()) },
    )
}

// A unique token corresponding to an event in a selector
type Token = usize;

// Used to signal to selectors that an event is ready
struct SelectorSignal {
    wait_lock: Mutex<Option<usize>>,
    trigger: Condvar,
    //listeners: AtomicUsize,
}
