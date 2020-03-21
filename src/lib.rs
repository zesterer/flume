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
    sync::{Arc, Condvar, Mutex, atomic::{AtomicUsize, Ordering}},
    time::{Duration, Instant},
    cell::{UnsafeCell, RefCell},
    marker::PhantomData,
    thread,
};
#[cfg(feature = "async")]
use std::task::Waker;
#[cfg(feature = "select")]
use crate::select::{Token, SelectorSignal};
#[cfg(feature = "async")]
use crate::r#async::RecvFuture;

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

struct Inner<T> {
    /// The internal queue
    queue: VecDeque<T>,
    /// In ID generator for sender selectors so we can keep track of them
    #[cfg(feature = "select")]
    send_selector_counter: usize,
    /// Used to waken sender selectors
    #[cfg(feature = "select")]
    send_selectors: Vec<(usize, Arc<SelectorSignal>, Token)>,
    /// Used to waken a receiver selector
    #[cfg(feature = "select")]
    recv_selector: Option<(Arc<SelectorSignal>, Token)>,
    /// Used to waken an async receiver task
    #[cfg(feature = "async")]
    recv_waker: Option<Waker>,
    /// The number of senders associated with this channel. If this drops to 0, the channel is
    /// 'dead' and the listener will begin reporting disconnect errors (once the queue has been
    /// drained).
    sender_count: usize,
    /// The number of senders waiting for notifications that the queue has space.
    send_waiters: usize,
    /// Used to describe the state of the receiving end of the queue:
    /// - 0 => Receiver has been dropped, so the channel is 'dead'
    /// - 1 => Receiver still exists, but is not waiting for notifications
    /// - x => Receiver is waiting for incoming message notifications
    listen_mode: usize,
}

struct Shared<T> {
    // Mutable state
    inner: spin::Mutex<Inner<T>>,
    /// Mutexed used for locking condvars.
    wait_lock: Mutex<()>,
    /// Used for notifying the receiver about incoming messages.
    send_trigger: Condvar,
    // Used for notifying senders about the queue no longer being full. Therefore, this is only a
    // `Some` for bounded queues.
    recv_trigger: Option<Condvar>,
    // Maximum queue capacity
    space_left: Option<AtomicUsize>,
}

impl<T> Shared<T> {
    fn new(cap: Option<usize>) -> Self {
        Self {
            inner: spin::Mutex::new(Inner {
                queue: VecDeque::with_capacity(cap.unwrap_or(0)), // TODO: Is this ok?

                #[cfg(feature = "select")]
                send_selector_counter: 0,
                #[cfg(feature = "select")]
                send_selectors: Vec::new(),
                #[cfg(feature = "select")]
                recv_selector: None,

                #[cfg(feature = "async")]
                recv_waker: None,

                sender_count: 1,
                send_waiters: 0,
                listen_mode: 1,
            }),
            space_left: cap.map(AtomicUsize::new),
            wait_lock: Mutex::new(()),
            send_trigger: Condvar::new(),
            recv_trigger: None,
        }
    }

    #[inline]
    fn with_inner<'a, R>(&'a self, f: impl FnOnce(spin::MutexGuard<'a, Inner<T>>) -> R) -> R {
        let mut i = 0;
        loop {
            for _ in 0..5 {
                if let Some(inner) = self.inner.try_lock() {
                    return f(inner);
                }
                thread::yield_now();
            }
            i += 1;
            thread::sleep(Duration::from_nanos(i * 250));
        }
    }

    #[inline]
    fn wait_inner(&self) -> spin::MutexGuard<'_, Inner<T>> {
        let mut i = 0;
        loop {
            for _ in 0..5 {
                if let Some(inner) = self.inner.try_lock() {
                    return inner;
                }
                thread::yield_now();
            }
            i += 1;
            thread::sleep(Duration::from_nanos(i * 250));
        }
    }

    #[inline]
    fn poll_inner(&self) -> Option<spin::MutexGuard<'_, Inner<T>>> {
        self.inner.try_lock()
    }

    #[inline]
    fn try_send(&self, msg: T) -> Result<(), (spin::MutexGuard<Inner<T>>, TrySendError<T>)> {
        if let Some(space_left) = self.space_left.as_ref() {
            'outer: loop {
                let mut space = space_left.load(Ordering::Relaxed);
                while space > 0 {
                    match space_left.compare_exchange_weak(
                        space,
                        space - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break 'outer,
                        Err(e) => space = e,
                    }
                }

                return Err((self.wait_inner(), TrySendError::Full(msg)));
            }
        }

        self.with_inner(|mut inner| {
            if inner.listen_mode == 0 {
                // If the listener has disconnected, the channel is dead
                return Err((inner, TrySendError::Disconnected(msg)));
            }

            inner.queue.push_back(msg);

            // TODO: Move this below the listen_mode check by making selectors listen-aware
            #[cfg(feature = "select")]
            {
                // Notify the receiving selector
                if let Some((signal, token)) = &inner.recv_selector {
                    let mut guard = signal.wait_lock.lock().unwrap();
                    // TODO: Can we get away with not having this here? Seems like it
                    //drop(inner); // Avoid a deadlock
                    *guard = Some(*token);
                    signal.trigger.notify_one();
                    return Ok(());
                }
            }

            // If nothing is listening, exit early
            if inner.listen_mode < 2 {
                return Ok(());
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
            drop(inner); // Avoid a deadlock
            let _ = self.wait_lock.lock().unwrap();
            self.send_trigger.notify_one();

            Ok(())
        })
    }

    #[inline]
    fn send(&self, mut msg: T) -> Result<(), SendError<T>> {
        loop {
            // Attempt to send a message
            let mut inner = match self.try_send(msg) {
                Ok(()) => return Ok(()),
                Err((_, TrySendError::Disconnected(msg))) => return Err(SendError(msg)),
                Err((inner, TrySendError::Full(m))) => {
                    msg = m;
                    inner
                },
            };

            if let Some(recv_trigger) = self.recv_trigger.as_ref() {
                // Take a guard of the main lock to use later when waiting
                let guard = self.wait_lock.lock().unwrap();
                // Inform the receiver that we need waking
                inner.send_waiters += 1;
                // We keep the queue alive until here to avoid a deadlock
                drop(inner);
                // Wait until we get a signal that suggests the queue might have space
                let _ = recv_trigger.wait(guard).unwrap();
                // Inform the receiver that we no longer need waking
                self.with_inner(|mut inner| inner.send_waiters -= 1);
            }
        }
    }

    /// Inform the receiver that all senders have been dropped
    #[inline]
    fn all_senders_disconnected(&self) {
        let _ = self.wait_lock.lock().unwrap();
        self.send_trigger.notify_all(); // TODO: notify_one instead? Which is faster?
        #[cfg(feature = "async")]
        {
            if let Some(recv_waker) = &self.inner.lock().recv_waker {
                recv_waker.wake_by_ref();
            }
        }
    }

    #[inline]
    fn receiver_disconnected(&self) {
        if let Some(recv_trigger) = self.recv_trigger.as_ref() {
            let _ = self.wait_lock.lock().unwrap();
            recv_trigger.notify_all();
        }
    }

    #[inline]
    fn take_remaining(&self) -> VecDeque<T> {
        self.with_inner(|mut inner| std::mem::take(&mut inner.queue))
    }

    #[inline]
    fn try_recv<'a>(
        &'a self,
        mut take_inner: impl FnMut() -> spin::MutexGuard<'a, Inner<T>>,
        buf: &mut VecDeque<T>,
    ) -> Result<T, (spin::MutexGuard<Inner<T>>, TryRecvError)> {
        let mut inner = None;
        loop {
            if let Some(msg) = buf.pop_front() {
                if self.space_left.as_ref().map(|sl| sl.fetch_add(1, Ordering::Relaxed)) == Some(0) {
                    let mut inner = inner.unwrap_or_else(take_inner);

                    #[cfg(feature = "select")]
                    {
                        // Notify send selectors
                        inner
                            .send_selectors
                            .iter()
                            .for_each(|(_, signal, token)| {
                                let mut guard = signal.wait_lock.lock().unwrap();
                                *guard = Some(*token);
                                signal.trigger.notify_one();
                            });
                    }

                    if inner.send_waiters > 0 {
                        drop(inner);
                        let _ = self.wait_lock.lock().unwrap();
                        self.recv_trigger.as_ref().unwrap().notify_one();
                    }
                }
                return Ok(msg)
            } else {
                inner = Some(inner.unwrap_or_else(&mut take_inner));
                // Swap buffers
                let mut inner_ref = inner.as_mut().unwrap();
                if inner_ref.queue.len() == 0 {
                    if inner_ref.sender_count == 0 {
                        return Err((inner.unwrap(), TryRecvError::Disconnected));
                    } else {
                        return Err((inner.unwrap(), TryRecvError::Empty));
                    }
                } else {
                    std::mem::swap(&mut inner_ref.queue, buf);
                }
            }
        }

        // let msg = match inner.queue.pop_front() {
        //     Some(msg) => msg,
        //     // If there's nothing more in the queue, this might be because there are no senders
        //     None if inner.sender_count == 0 =>
        //         return Err((inner, TryRecvError::Disconnected)),
        //     None => return Err((inner, TryRecvError::Empty)),
        // };

        // // Swap the buffers to grab the messages
        // std::mem::swap(&mut inner_ref.queue, buf);

        // #[cfg(feature = "select")]
        // {
        //     // Notify send selectors
        //     inner
        //         .send_selectors
        //         .iter()
        //         .for_each(|(_, signal, token)| {
        //             let mut guard = signal.wait_lock.lock().unwrap();
        //             *guard = Some(*token);
        //             signal.trigger.notify_one();
        //         });
        // }

        // If there are senders waiting for a message, wake them up.
        // if let Some(recv_trigger) = self.recv_trigger.as_ref() {
        //     if inner.queue.is_bounded() && inner.send_waiters > 0 {
        //         drop(inner);
        //         let _ = self.wait_lock.lock().unwrap();
        //         recv_trigger.notify_one();
        //     }
        // }

        //Ok(msg)
    }

    #[inline]
    fn recv(
        &self,
        buf: &mut VecDeque<T>,
    ) -> Result<T, RecvError> {
        loop {
            // Attempt to receive a message
            let mut i = 0;
            let mut inner = loop {
                match self.try_recv(|| self.wait_inner(), buf) {
                    Ok(msg) => return Ok(msg),
                    Err((_, TryRecvError::Disconnected)) => return Err(RecvError::Disconnected),
                    Err((inner, TryRecvError::Empty)) if i == 3 => break inner,
                    Err((_, TryRecvError::Empty)) => {},
                };
                thread::yield_now();
                i += 1;
            };

            // Take a guard of the main lock to use later when waiting
            let guard = self.wait_lock.lock().unwrap();
            // Inform the sender that we need waking
            inner.listen_mode = 2;
            // We keep the queue alive until here to avoid a deadlock
            drop(inner);
            // Wait until we get a signal that the queue has new messages
            let _ = self.send_trigger.wait(guard).unwrap();
            // Inform the sender that we no longer need waking
            self.with_inner(|mut inner| inner.listen_mode = 1);
        }
    }

    // TODO: Change this to `recv_timeout` to potentially avoid an extra call to `Instant::now()`?
    #[inline]
    fn recv_deadline(
        &self,
        deadline: Instant,
        buf: &mut VecDeque<T>,
    ) -> Result<T, RecvTimeoutError> {
        // Attempt a speculative recv. If we are lucky there might be a message in the queue!
        if let Ok(msg) = self.try_recv(|| self.wait_inner(), buf) {
            return Ok(msg);
        }

        // Inform senders that we're going into a listening period and need to be notified of new
        // messages.
        self.with_inner(|mut inner| inner.listen_mode = 2);

        let mut guard = self.wait_lock.lock().unwrap();
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
            match self.try_recv(|| self.wait_inner(), buf) {
                Ok(msg) => break Ok(msg),
                Err((_, TryRecvError::Empty)) => {},
                Err((_, TryRecvError::Disconnected)) => break Err(RecvTimeoutError::Disconnected),
            }
        };
        // Ensure the listen mode is reset
        self.with_inner(|mut inner| inner.listen_mode = 1);
        result
    }

    #[cfg(feature = "select")]
    #[inline]
    fn connect_send_selector(&self, signal: Arc<SelectorSignal>, token: Token) -> usize {
        let mut inner = self.inner.lock();
        inner.send_selector_counter += 1;
        let id = inner.send_selector_counter;
        inner.send_selectors.push((id, signal, token));
        id
    }

    #[cfg(feature = "select")]
    #[inline]
    fn disconnect_send_selector(&self, id: usize) {
        self.inner.lock().send_selectors.retain(|(s_id, _, _)| s_id != &id);
    }

    #[cfg(feature = "select")]
    #[inline]
    fn connect_recv_selector(&self, signal: Arc<SelectorSignal>, token: Token) {
        self.inner.lock().recv_selector = Some((signal, token));
    }

    #[cfg(feature = "select")]
    #[inline]
    fn disconnect_recv_selector(&self) {
        self.inner.lock().recv_selector = None;
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
        self.shared.with_inner(|mut inner| inner.sender_count += 1);
        //self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Notify the receiver that all senders have been dropped if the number of senders drops
        // to 0.
        if self.shared.with_inner(|mut inner| {
            inner.sender_count -= 1;
            inner.sender_count
        }) == 0 {
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
}

impl<T> Receiver<T> {
    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv(&mut self.buffer.borrow_mut())
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the timeout has expired.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(
            Instant::now().checked_add(timeout).unwrap(),
            &mut self.buffer.borrow_mut()
        )
    }

    /// Wait for an incoming value from the channel associated with this receiver, returning an
    /// error if all channel senders have been dropped or the deadline has passed.
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(deadline, &mut self.buffer.borrow_mut())
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
            .try_recv(|| self.shared.wait_inner(), &mut self.buffer.borrow_mut())
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
        Drain { queue: self.buffer.borrow_mut().drain(..).chain(self.shared.take_remaining().into_iter()).collect(), _phantom: PhantomData }
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
        self.shared.with_inner(|mut inner| inner.listen_mode = 0);
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
            _phantom_cell: UnsafeCell::new(())
        },
    )
}
