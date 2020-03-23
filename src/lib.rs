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
    sync::{Arc, Condvar, Mutex, WaitTimeoutResult, atomic::{AtomicUsize, Ordering}},
    time::{Duration, Instant},
    cell::{UnsafeCell, RefCell},
    marker::PhantomData,
    thread,
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
use std::cell::Cell;

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

#[derive(Default)]
struct Signal<T = ()> {
    lock: Mutex<T>,
    trigger: Condvar,
    waiters: AtomicUsize,
}

impl<T> Signal<T> {
    fn wait<G>(&self, sync_guard: G) {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let _guard = self.trigger.wait(guard).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }

    fn do_then_wait_while<G>(&self, sync_guard: G, first: impl FnOnce(&mut T), cond: impl FnMut(&mut T) -> bool) {
        let mut guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        first(&mut *guard);
        let _guard = self.trigger.wait_while(guard, cond).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }

    fn wait_then<G, R>(&self, sync_guard: G, then: impl FnOnce(&mut T) -> R) -> R {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let mut guard = self.trigger.wait(guard).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
        then(&mut *guard)
    }

    fn wait_timeout<G>(&self, dur: Duration, sync_guard: G) -> WaitTimeoutResult {
        let guard = self.lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::Relaxed);
        drop(sync_guard);
        let (_guard, timeout) = self.trigger.wait_timeout(guard, dur).unwrap();
        self.waiters.fetch_sub(1, Ordering::Relaxed);
        timeout
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

/// Wrapper around a queue. This wrapper exists to permit a maximum length.
struct Queue<T>(VecDeque<T>, Option<usize>);

impl<T> Queue<T> {
    fn new(cap: Option<usize>) -> Self { Self(VecDeque::new(), cap) }

    fn push(&mut self, x: T) -> Result<(), T> {
        if self.1 == Some(self.0.len()) {
            Err(x)
        } else {
            self.0.push_back(x);
            Ok(())
        }
    }

    fn pop(&mut self) -> Option<T> {
        self.0.pop_front()
    }

    fn swap(&mut self, buf: &mut VecDeque<T>) {
        // Swapping on bounded queues doesn't work correctly since it gives senders a false
        // impression of how many items are in the queue, allowing them to push too many items into
        // the queue. After some experimentation, it seems like the overhead of tracking the length
        // is greater than the performance to be gained by swapping only in unbounded channels.
        if !self.1.is_some() {
            std::mem::swap(&mut self.0, buf);
        }
    }

    fn take(&mut self) -> Self {
        std::mem::replace(self, Queue(VecDeque::new(), self.1))
    }
}

struct Inner<T> {
    /// The internal queue
    queue: Queue<T>,
    /// In ID generator for sender selectors so we can keep track of them
    #[cfg(feature = "select")]
    send_selector_counter: usize,
    /// Used to waken sender selectors
    #[cfg(feature = "select")]
    send_selectors: Vec<(usize, Arc<Signal<Token>>, Token)>,
    /// Used to waken a receiver selector
    #[cfg(feature = "select")]
    recv_selector: Option<(Arc<Signal<Token>>, Token)>,
    /// Used to waken an async receiver task
    #[cfg(feature = "async")]
    recv_waker: Option<Waker>,
    /// The number of senders associated with this channel. If this drops to 0, the channel is
    /// 'dead' and the listener will begin reporting disconnect errors (once the queue has been
    /// drained).
    sender_count: usize,
    /// Used to describe the state of the receiving end of the queue:
    /// - 0 => Receiver has been dropped, so the channel is 'dead'
    /// - 1 => Receiver still exists, but is not waiting for notifications
    /// - x => Receiver is waiting for incoming message notifications
    listen_mode: usize,
}

struct Shared<T> {
    // Mutable state
    inner: InnerMutex<Inner<T>>,
    /// Used for notifying the receiver about incoming messages.
    send_signal: Signal,
    // Used for notifying senders about the queue no longer being full. Therefore, this is only a
    // `Some` for bounded queues.
    recv_signal: Option<Signal>,
    rendezvous_signal: Option<Signal<Option<T>>>,
}

impl<T> Shared<T> {
    fn new(cap: Option<usize>) -> Self {
        Self {
            inner: InnerMutex::new(Inner {
                queue: Queue::new(cap),

                #[cfg(feature = "select")]
                send_selector_counter: 0,
                #[cfg(feature = "select")]
                send_selectors: Vec::new(),
                #[cfg(feature = "select")]
                recv_selector: None,

                #[cfg(feature = "async")]
                recv_waker: None,

                sender_count: 1,
                listen_mode: 1,
            }),
            send_signal: Signal::default(),
            recv_signal: if cap.is_some() { Some(Signal::default()) } else { None },
            rendezvous_signal: if cap == Some(0) { Some(Signal::default()) } else { None },
        }
    }

    #[inline]
    fn lock_inner(&self) -> MutexGuard<Inner<T>> {
        #[cfg(windows)] { self.inner.lock().unwrap() }
        #[cfg(not(windows))] { self.inner.lock() }
    }

    #[inline]
    #[cfg(not(windows))]
    fn wait_inner(&self) -> MutexGuard<'_, Inner<T>> {
        let mut i = 0;
        loop {
            for _ in 0..5 {
                if let Some(inner) = self.inner.try_lock() {
                    return inner;
                }
                thread::yield_now();
            }
            thread::sleep(Duration::from_nanos(i * 50));
            i += 1;
        }
    }

    #[inline]
    #[cfg(windows)]
    fn wait_inner(&self) -> MutexGuard<'_, Inner<T>> {
        self.lock_inner()
    }

    #[inline]
    #[cfg(feature = "async")]
    fn poll_inner(&self) -> Option<MutexGuard<'_, Inner<T>>> {
        #[cfg(windows)] { self.inner.try_lock().ok() }
        #[cfg(not(windows))] { self.inner.try_lock() }
    }

    #[inline]
    fn try_send(&self, msg: T) -> Result<(), (MutexGuard<Inner<T>>, TrySendError<T>)> {
        let mut inner = self.wait_inner();

        if inner.listen_mode == 0 {
            // If the listener has disconnected, the channel is dead
            return Err((inner, TrySendError::Disconnected(msg)));
        }
        // If pushing fails, it's because the queue is full
        match inner.queue.push(msg) {
            Err(msg) => return Err((inner, TrySendError::Full(msg))),
            Ok(()) => {},
        };

        // TODO: Move this below the listen_mode check by making selectors listen-aware
        #[cfg(feature = "select")]
        {
            // Notify the receiving selector
            if let Some((signal, token)) = &inner.recv_selector {
                signal.notify_one_with(|t| *t = *token, ());
            }
        }

        // TODO: Have a different listen mode for async vs sync receivers?
        #[cfg(feature = "async")]
        {
            // Notify the receiving async task
            if let Some(recv_waker) = &inner.recv_waker {
                recv_waker.wake_by_ref();
            }
        }

        // Notify the receiver of a new message
        self.send_signal.notify_one(inner);
        Ok(())
    }

    #[inline]
    fn send(&self, mut msg: T) -> Result<(), SendError<T>> {
        loop {
            // Attempt to send a message
            let inner = match self.try_send(msg) {
                Ok(()) => return Ok(()),
                Err((_, TrySendError::Disconnected(msg))) => return Err(SendError(msg)),
                Err((inner, TrySendError::Full(m))) => if let Some(sig) = self.rendezvous_signal.as_ref() {
                    sig.do_then_wait_while(inner, |msg| {
                        *msg = Some(m);
                        // Notify the receiver of a new rendezvous message
                        self.send_signal.notify_one(());
                    }, |msg| msg.is_some());
                    return Ok(());
                } else {
                    msg = m;
                    inner
                },
            };

            if let Some(recv_signal) = self.recv_signal.as_ref() {
                // Wait until we get a signal that suggests the queue might have space
                recv_signal.wait(inner);
            }
        }
    }

    /// Inform the receiver that all senders have been dropped
    #[inline]
    fn all_senders_disconnected(&self) {
        self.send_signal.notify_all(self.inner.lock());
        #[cfg(feature = "async")]
        {
            if let Some(recv_waker) = &self.lock_inner().recv_waker {
                recv_waker.wake_by_ref();
            }
        }
    }

    #[inline]
    fn receiver_disconnected(&self) {
        if let Some(recv_signal) = self.recv_signal.as_ref() {
            recv_signal.notify_all(self.inner.lock());
        }
    }

    #[inline]
    fn take_remaining(&self) -> Queue<T> {
        self.wait_inner().queue.take()
    }

    #[inline]
    fn try_recv<'a>(
        &'a self,
        take_inner: impl FnOnce() -> MutexGuard<'a, Inner<T>>,
        buf: &mut VecDeque<T>,
        finished: &Cell<bool>,
    ) -> Result<T, (MutexGuard<Inner<T>>, TryRecvError)> {
        // Eagerly check the buffer
        if let Some(msg) = buf.pop_front() {
            return Ok(msg);
        }

        let mut inner = take_inner();

        // Rendezvous behaviour
        if let Some(rendezvous_signal) = self.rendezvous_signal.as_ref() {
            let mut msg = None;
            rendezvous_signal.notify_one_with(|m| msg = m.take(), ());
            if let Some(msg) = msg {
                return Ok(msg);
            } else {
                return Err((inner, TryRecvError::Empty));
            }
        }

        let msg = match inner.queue.pop() {
            Some(msg) => msg,
            // If there's nothing more in the queue, this might be because there are no senders
            None if inner.sender_count == 0 => {
                finished.set(true);
                return Err((inner, TryRecvError::Disconnected));
            },
            None => return Err((inner, TryRecvError::Empty)),
        };

        // Swap the buffers to grab the messages
        inner.queue.swap(buf);

        #[cfg(feature = "select")]
        {
            // Notify send selectors
            inner
                .send_selectors
                .iter()
                .for_each(|(_, signal, token)| {
                    signal.notify_one_with(|t| *t = *token, ());
                });
        }

        // If there are senders waiting for a message, wake them up.
        if let Some(recv_signal) = self.recv_signal.as_ref() {
            // Notify the receiver of a new message
            recv_signal.notify_one(inner);
        }

        Ok(msg)
    }

    #[inline]
    fn recv(
        &self,
        buf: &mut VecDeque<T>,
        finished: &Cell<bool>,
    ) -> Result<T, RecvError> {
        loop {
            // Attempt to receive a message
            let mut i = 0;
            let inner = loop {
                match self.try_recv(|| self.wait_inner(), buf, finished) {
                    Ok(msg) => return Ok(msg),
                    Err((_, TryRecvError::Disconnected)) => return Err(RecvError::Disconnected),
                    Err((inner, TryRecvError::Empty)) if i == 3 => break inner,
                    Err((_, TryRecvError::Empty)) => {},
                };
                thread::yield_now();
                i += 1;
            };

            // Wait until we get a signal that the queue has new messages
            self.send_signal.wait(inner);
        }
    }

    // TODO: Change this to `recv_timeout` to potentially avoid an extra call to `Instant::now()`?
    #[inline]
    fn recv_deadline(
        &self,
        deadline: Instant,
        buf: &mut VecDeque<T>,
        finished: &Cell<bool>,
    ) -> Result<T, RecvTimeoutError> {
        // Attempt a speculative recv. If we are lucky there might be a message in the queue!
        let mut inner = match self.try_recv(|| self.wait_inner(), buf, finished) {
            Ok(msg) => return Ok(msg),
            Err((_, TryRecvError::Disconnected)) => return Err(RecvTimeoutError::Disconnected),
            Err((inner, TryRecvError::Empty)) => inner,
        };

        loop {
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
            let timeout = self.send_signal.wait_timeout(timeout, inner);
            if timeout.timed_out() {
                // This was a timeout rather than a wakeup, so produce a timeout error.
                break Err(RecvTimeoutError::Timeout);
            }

            // Attempt to receive a message from the queue
            inner = match self.try_recv(|| self.wait_inner(), buf, finished) {
                Ok(msg) => return Ok(msg),
                Err((inner, TryRecvError::Empty)) => inner,
                Err((_, TryRecvError::Disconnected)) => return Err(RecvTimeoutError::Disconnected),
            };
        }
    }

    #[cfg(feature = "select")]
    #[inline]
    fn connect_send_selector(&self, signal: Arc<Signal<Token>>, token: Token) -> usize {
        let mut inner = self.lock_inner();
        inner.send_selector_counter += 1;
        let id = inner.send_selector_counter;
        inner.send_selectors.push((id, signal, token));
        id
    }

    #[cfg(feature = "select")]
    #[inline]
    fn disconnect_send_selector(&self, id: usize) {
        self.lock_inner().send_selectors.retain(|(s_id, _, _)| s_id != &id);
    }

    #[cfg(feature = "select")]
    #[inline]
    fn connect_recv_selector(&self, signal: Arc<Signal<Token>>, token: Token) {
        self.lock_inner().recv_selector = Some((signal, token));
    }

    #[cfg(feature = "select")]
    #[inline]
    fn disconnect_recv_selector(&self) {
        self.lock_inner().recv_selector = None;
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
        self.shared.try_send(msg).map(|_| ()).map_err(|(_, err)| err)
    }
}

impl<T> Clone for Sender<T> {
    /// Clone this sender. [`Sender`] acts as a handle to a channel, and the channel will only be
    /// cleaned up when all senders and the receiver have been dropped.
    fn clone(&self) -> Self {
        self.shared.wait_inner().sender_count += 1;
        //self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Notify the receiver that all senders have been dropped if the number of senders drops
        // to 0.
        if {
            let mut inner = self.shared.wait_inner();
            inner.sender_count -= 1;
            inner.sender_count
        } == 0 {
            self.shared.all_senders_disconnected();
        }
    }
}

/// The receiving end of a channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    /// Buffer for messages
    buffer: RefCell<VecDeque<T>>,
    /// Used to prevent Sync being implemented for this type - we never actually use it!
    /// TODO: impl<T> !Sync for Receiver<T> {} when negative traits are stable
    _phantom_cell: UnsafeCell<()>,
    /// Whether all receivers have disconnected and there are no messages in any buffer
    finished: Cell<bool>,
}

impl<T> Receiver<T> {
    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv(&mut self.buffer.borrow_mut(), &self.finished)
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the timeout has expired.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(
            Instant::now().checked_add(timeout).unwrap(),
            &mut self.buffer.borrow_mut(),
            &self.finished
        )
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(
            deadline,
            &mut self.buffer.borrow_mut(),
            &self.finished
        )
    }

    // Takes `&mut self` to avoid >1 task waiting on this channel
    // TODO: Is this necessary?
    /// Create a future that may be used to wait asynchronously for an incoming value on the
    /// channel associated with this receiver.
    #[cfg(feature = "async")]
    pub fn recv_async(&mut self) -> RecvFuture<T> {
        RecvFuture::new(self)
    }

    /// Attempt to fetch an incoming value from the channel associated with this receiver,
    /// returning an error if the channel is empty or all channel senders have been dropped.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self
            .shared
            .try_recv(
                || self.shared.wait_inner(),
                &mut self.buffer.borrow_mut(),
                &self.finished
            )
            .map_err(|(_, err)| err)
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
        let mut queue = std::mem::replace(&mut *self.buffer.borrow_mut(), VecDeque::new());
        queue.append(&mut self.shared.take_remaining().0);
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
        self.shared.wait_inner().listen_mode = 0;
        self.shared.receiver_disconnected();
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
        Receiver {
            shared,
            buffer: RefCell::new(VecDeque::new()),
            finished: Cell::new(false),
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
        Sender { shared: shared.clone() },
        Receiver {
            shared,
            buffer: RefCell::new(VecDeque::new()),
            finished: Cell::new(false),
            _phantom_cell: UnsafeCell::new(())
        },
    )
}
