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
    /// Mutex and Condvar used for notifying the receiver about incoming messages. The inner bool is
    /// used to indicate that all senders have been dropped and that the channel is 'dead'.
    wait_lock: Mutex<bool>,
    send_trigger: Condvar,
    recv_trigger: Condvar,
    /// The number of senders associated with this channel. If this drops to 0, the channel is
    /// 'dead' and the listener will begin reporting disconnect errors (once the queue has been
    /// drained).
    senders: AtomicUsize,
    send_waiters: AtomicUsize,
    /// An atomic used to describe the state of the receiving end of the queue
    /// - 0 => Receiver has been dropped, so the channel is 'dead'
    /// - 1 => Receiver still exists, but is not waiting for notifications
    /// - x => Receiver is waiting for incoming message notifications
    listen_mode: AtomicUsize,
}

impl<T> Shared<T> {
    fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        loop {
            // Attempt to lock the queue. Upon success, attempt to receive. If the queue is empty,
            // we don't block anyway so just break out of the loop.
            if let Some(mut queue) = self.queue.try_lock() {
                let listen_mode = self.listen_mode.load(Ordering::Relaxed);
                // If the listener has disconnected, the channel is dead
                if listen_mode == 0 {
                    break Err(TrySendError::Disconnected(msg));
                } else {
                    // If pushing fails, it's because the queue is full
                    if let Some(msg) = queue.push(msg) {
                        break Err(TrySendError::Full(msg));
                    } else if listen_mode > 1 {
                        // Notify the receiver of a new message if listeners are waiting
                        let _ = self.wait_lock.lock().unwrap();
                        // Drop the queue early to avoid a deadlock
                        drop(queue);
                        self.send_trigger.notify_one();
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
            // Attempt to gain exclusive access to the queue
            let guard = if let Some(mut queue) = self.queue.try_lock() {
                let listen_mode = self.listen_mode.load(Ordering::Relaxed);
                // If list_mode is 0, it means that the receiver has been dropped
                if listen_mode == 0 {
                    break Err(SendError(msg));
                } else {
                    if let Some(m) = queue.push(msg) {
                        // If pushing fails, the queue is full: if this is the case, we increment
                        // send_waiters to inform the receiver that we need a notification, and then we
                        // take a lock guard to wait on later using the condvar.
                        msg = m;
                        self.send_waiters.fetch_add(1, Ordering::Acquire);
                        Some(self.wait_lock.lock().unwrap())
                    } else if listen_mode > 1 {
                        // If listen_mode is greater than one it means that the receiver is passively
                        // waiting on a notification that new items have entered the queue.
                        let _ = self.wait_lock.lock().unwrap();
                        // Drop the queue early to avoid deadlocking when notifying the receiver.
                        drop(queue);
                        // Tell the receiver to wake up
                        self.send_trigger.notify_one();
                        break Ok(());
                    } else {
                        break Ok(());
                    }
                }
            } else {
                None
            };

            // If the receiver has disconnected
            if guard
                .as_ref()
                .map(|guard| **guard)
                .unwrap_or(false)
            {
                // Normally we would decrement send_waiters here, but since the receiver has
                // disconnected anyway, it doesn't need to know that we're not waiting any more.
                break Err(SendError(msg));
            }

            if let Some(g) = guard {
                let _ = self.recv_trigger.wait(g).unwrap();
                self.send_waiters.fetch_sub(1, Ordering::Release);
            } else {
                thread::yield_now();
            }
        }
    }

    /// Inform the receiver that all senders have been dropped
    fn all_senders_disconnected(&self) {
        let mut disconnected = self.wait_lock.lock().unwrap();
        *disconnected = true;
        self.send_trigger.notify_all(); // TODO: notify_one instead? Which is faster?
    }

    fn receiver_disconnected(&self) {
        let mut disconnected = self.wait_lock.lock().unwrap();
        *disconnected = true;
        self.recv_trigger.notify_all();
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        loop {
            // Attempt to lock the queue. Upon success, attempt to receive. If the queue is empty,
            // we don't block anyway so just break out of the loop.
            if let Some(mut queue) = self.queue.try_lock() {
                break if let Some(msg) = queue.pop() {
                    // If there are senders waiting for a message, wake them up.
                    if self.send_waiters.load(Ordering::Relaxed) > 0 {
                        let _ = self.wait_lock.lock().unwrap();
                        drop(queue);
                        self.recv_trigger.notify_one();
                    }
                    Ok(msg)
                } else if self.senders.load(Ordering::Relaxed) == 0 {
                    // If there's nothing more in the queue, this might be because there are no
                    // more senders.
                    Err(TryRecvError::Disconnected)
                } else {
                    Err(TryRecvError::Empty)
                };
            } else {
                // If we can't gain access to the queue, yield until the next time slice
                thread::yield_now();
            }
        }
    }

    fn recv(&self) -> Result<T, RecvError> {
        let mut disconnected = false;

        let result = loop {
            // Attempt to gain exclusive access to the queue
            let guard = if let Some(mut queue) = self.queue.try_lock() {
                match queue.pop() {
                    // We've got the message we wanted
                    Some(msg) => {
                        if queue.1.is_some() && self.send_waiters.load(Ordering::Relaxed) > 0 {
                            let _ = self.wait_lock.lock().unwrap();
                            drop(queue);
                            self.recv_trigger.notify_one();
                        }
                        break Ok(msg)
                    },
                    // No messages left, and there are no more senders, so our work here is done.
                    None if disconnected => break Err(RecvError::Disconnected),
                    // Sleep when empty
                    None => {
                        // Indicate to future senders that we'll need to be woken up since we're
                        // going to wait upon the condvar trigger.
                        self.listen_mode.store(2, Ordering::Relaxed);
                        // Take a guard of the mutex, but do so while the queue is still locked.
                        // This prevents a deadlock situation occurring.
                        Some(self.wait_lock.lock().unwrap())
                    },
                }
            } else {
                None
            };

            // Check to see whether senders still exist
            disconnected |= guard
                .as_ref()
                .map(|guard| **guard)
                .unwrap_or(false);

            // If the channel is still connected and we've got access to the guard, wait.
            if let (Some(g), false) = (guard, disconnected) {
                // Sleep using the guard we took while probing the queue if the queue is empty
                let _ = self.send_trigger.wait(g).unwrap();
            } else {
                // If the queue isn't accessible, assume that it's in use and yield so that
                // whatever is using it can do its work.
                thread::yield_now();
            }
        };
        // Ensure the listen mode is reset
        // TODO: Are we performing more atomic stores than we need to here?
        self.listen_mode.store(1, Ordering::Release);
        result
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
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => break Err(RecvTimeoutError::Disconnected),
            }
        };
        // Ensure the listen mode is reset
        self.listen_mode.store(1, Ordering::Relaxed);
        result
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
        self.shared.try_send(msg)
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
        self.shared.try_recv()
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
        wait_lock: Mutex::new(false),
        send_trigger: Condvar::new(),
        recv_trigger: Condvar::new(),
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
        wait_lock: Mutex::new(false),
        send_trigger: Condvar::new(),
        recv_trigger: Condvar::new(),
        senders: AtomicUsize::new(1),
        send_waiters: AtomicUsize::new(0),
        listen_mode: AtomicUsize::new(1),
    });
    (
        Sender { shared: shared.clone() },
        Receiver { shared, _phantom_cell: UnsafeCell::new(()) },
    )
}
