///! # Flume
/// !
///! A blazingly fast multi-producer, single-consumer channel.
///!
///! ## Examples
///!
///! ```
///! let (tx, rx) = flume::channel::<u32>();
///!
///! tx.send(42).unwrap();
///! assert_eq!(rx.recv().unwrap(), 42);
///! ```

use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, Ordering, spin_loop_hint}},
    time::{Duration, Instant},
    thread,
};
use std::sync::{Condvar, Mutex};

/// An error that may be emitted when attempting to send a value into a channel on a sender.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SendError<T: Send + 'static>(T);

/// An error that may be emitted when attempting to wait for a value on a receiver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
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

struct Queue<T> {
    inner: VecDeque<T>,
}

impl<T> Queue<T> {
    fn new() -> Self {
        Self { inner: VecDeque::new() }
    }

    fn push(&mut self, x: T) {
        self.inner.push_back(x);
    }

    fn pop(&mut self) -> Option<T> {
        self.inner.pop_front()
    }
}

struct Shared<T: Send + 'static> {
    queue: spin::Mutex<Queue<T>>,
    recv_lock: Mutex<bool>,
    send_trigger: Condvar,
    senders: AtomicUsize,
    listeners: AtomicUsize,
}

impl<T: Send + 'static> Shared<T> {
    fn send(&self, msg: T) -> Result<(), SendError<T>> {
        loop {
            if let Some(mut queue) = self.queue.try_lock() {
                let listen_mode = self.listeners.load(Ordering::Relaxed);
                if listen_mode == 0 {
                    break Err(SendError(msg));
                } else {
                    queue.push(msg);
                    drop(queue);
                    if listen_mode > 1 {
                        let _ = self.recv_lock.lock().unwrap();
                        self.send_trigger.notify_one();
                    }
                    break Ok(());
                }
            } else {
                thread::yield_now();
            }
        }
    }

    fn all_senders_disconnected(&self) {
        let mut disconnected = self.recv_lock.lock().unwrap();
        *disconnected = true;
        self.send_trigger.notify_all();
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        loop {
            if let Some(mut queue) = self.queue.try_lock() {
                break queue.pop().ok_or(TryRecvError::Empty);
            } if self.senders.load(Ordering::Relaxed) == 0 {
                break Err(TryRecvError::Disconnected);
            } else {
                thread::yield_now();
            }
        }
    }

    fn recv(&self) -> Result<T, RecvError> {
        let mut guard = None;
        let mut disconnected = false;

        let result = loop {
            guard = if let Some(mut queue) = self.queue.try_lock() {
                match queue.pop() {
                    Some(msg) => break Ok(msg),
                    None if disconnected => break Err(RecvError::Disconnected),
                    // Sleep when empty
                    None => {
                        self.listeners.store(2, Ordering::Relaxed);
                        Some(guard
                            .unwrap_or_else(|| self.recv_lock.lock().unwrap()))
                    },
                }
            } else {
                guard
            };

            disconnected |= guard
                .as_ref()
                .map(|guard| **guard)
                .unwrap_or(false);

            if let (Some(g), false) = (guard.take(), disconnected) {
                guard = Some(self.send_trigger.wait(g).unwrap());
            } else {
                thread::yield_now(); // Attempt a yield before sleeping
            }
            if guard.is_none() {
                self.listeners.store(1, Ordering::Relaxed);
            }
        };
        self.listeners.store(1, Ordering::Relaxed);
        result
    }

    fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        if let Ok(msg) = self.try_recv() {
            return Ok(msg);
        }

        let mut guard = self.recv_lock.lock().unwrap();
        self.listeners.store(2, Ordering::Relaxed);
        let result = loop {
            let now = Instant::now();
            let timeout = if now >= deadline {
                break Err(RecvTimeoutError::Timeout);
            } else {
                deadline.duration_since(now)
            };

            let (nguard, timeout) = self.send_trigger.wait_timeout(guard, timeout).unwrap();
            guard = nguard;
            if timeout.timed_out() {
                break Err(RecvTimeoutError::Timeout);
            }

            match self.try_recv() {
                Ok(msg) => break Ok(msg),
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => break Err(RecvTimeoutError::Disconnected),
            }
        };
        self.listeners.store(1, Ordering::Relaxed);
        result
    }
}

/// A transmitting end of a channel.
pub struct Sender<T: Send + 'static> {
    shared: Arc<Shared<T>>,
}

impl<T: Send + 'static> Sender<T> {
    /// Attempt to send a value into the channel, returning an error if the channel receiver has
    /// been dropped.
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.shared.send(msg)
    }
}

impl<T: Send + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T: Send + 'static> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.all_senders_disconnected();
        }
    }
}

/// The receiving end of a channel.
pub struct Receiver<T: Send + 'static> {
    shared: Arc<Shared<T>>,
}

impl<T: Send + 'static> Receiver<T> {
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
        Iter { shared: &self.shared }
    }

    /// A non-blocking iterator over the values received on the channel that finishes iteration
    /// when all receivers of the channel have been dropped or the channel is empty.
    pub fn try_iter(&self) -> impl Iterator<Item=T> + '_ {
        TryIter { shared: &self.shared }
    }
}

impl<T: Send + 'static> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { receiver: self }
    }
}

impl<T: Send + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.listeners.fetch_sub(1, Ordering::Relaxed);
    }
}

/// An iterator over the items received from a channel.
pub struct Iter<'a, T: Send + 'static> {
    shared: &'a Shared<T>,
}

impl<'a, T: Send + 'static> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.shared.recv().ok()
    }
}

/// An non-blocking iterator over the items received from a channel.
pub struct TryIter<'a, T: Send + 'static> {
    shared: &'a Shared<T>,
}

impl<'a, T: Send + 'static> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        match self.shared.try_recv() {
            Ok(msg) => Some(msg),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

/// An owned iterator over the items received from a channel.
pub struct IntoIter<T: Send + 'static> {
    receiver: Receiver<T>,
}

impl<T: Send + 'static> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.recv().ok()
    }
}

/// Create a new channel.
///
/// Create an unbounded channel with a [`Sender`] and [`Receiver`] connected to each end
/// respectively. Values sent in one end of the channel will be received on the other end. The
/// channel is thread-safe, and both sender and receiver may be sent to threads as necessary. In
/// addition, [`Sender`] may be cloned.
///
/// # Examples
/// ```
/// let (tx, rx) = flume::channel::<u32>();
///
/// tx.send(42).unwrap();
/// assert_eq!(rx.recv().unwrap(), 42);
/// ```
pub fn channel<T: Send + 'static>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        queue: spin::Mutex::new(Queue::new()),
        recv_lock: Mutex::new(false),
        send_trigger: Condvar::new(),
        senders: AtomicUsize::new(1),
        listeners: AtomicUsize::new(1),
    });
    (
        Sender { shared: shared.clone() },
        Receiver { shared },
    )
}
