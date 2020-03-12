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
    sync::{Arc, atomic::{AtomicUsize, AtomicBool, Ordering}},
    time::{Duration, Instant},
};
use std::sync::{Condvar, Mutex, MutexGuard};

/// An error that may be emitted when attempting to send a value into a channel on a sender.
#[derive(Copy, Clone, Debug)]
pub struct SendError<T: Send + 'static>(T);

/// An error that may be emitted when attempting to wait for a value on a receiver.
#[derive(Copy, Clone, Debug)]
pub enum RecvError {
    Disconnected,
}

/// An error that may be emitted when attempting to fetch a value on a receiver.
#[derive(Copy, Clone, Debug)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

/// An error that may be emitted when attempting to wait for a value on a receiver with a timeout.
#[derive(Copy, Clone, Debug)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

fn backoff<T, U>(
    spin_max: u64,
    mut test: impl FnMut() -> Option<T>,
    success: impl FnOnce(T) -> U,
    mut wait: impl FnMut(),
) -> U {
    let mut spin_time = 1;
    loop {
        match test() {
            Some(r) => break success(r),
            None => if spin_time > spin_max {
                wait();
            } else {
                spin_sleep::sleep(Duration::from_nanos(1 << spin_time));
                spin_time += 1;
            },
        }
    }
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
    recv_lock: Mutex<()>,
    send_trigger: Condvar,
    is_disconnected: AtomicBool,
    senders: AtomicUsize,
    listen_mode: AtomicUsize,
}

impl<T: Send + 'static> Shared<T> {
    fn send(&self, msg: T) -> Result<(), SendError<T>> {
        backoff(
            8,
            || self.queue.try_lock(),
            |mut queue| {
                match self.listen_mode.load(Ordering::Relaxed) {
                    0 => return Err(SendError(msg)),
                    1 => {},
                    _ => self.send_trigger.notify_all(),
                }

                queue.push(msg);
                Ok(())
            },
            // If everything goes to hell and the stars align against us, make sure we don't wait
            // for too long before trying again.
            || spin_sleep::sleep(Duration::from_nanos(500)),
        )
    }

    fn is_disconnected(&self) -> bool {
        self.is_disconnected.load(Ordering::Relaxed)
    }

    fn all_senders_disconnected(&self) {
        self.is_disconnected.store(true, Ordering::Relaxed);
        let _ = self.recv_lock.lock().unwrap();
        self.send_trigger.notify_all();
    }

    fn wait<R>(&self, f: impl FnOnce(&Condvar, MutexGuard<()>) -> R) -> Option<R> {
        self.listen_mode.fetch_add(1, Ordering::Acquire);
        let r = {
            let recv_lock = self.recv_lock.lock().unwrap();

            if self.is_disconnected() {
                None
            } else {
                Some(f(&self.send_trigger, recv_lock))
            }
        };
        self.listen_mode.fetch_sub(1, Ordering::Release);
        r
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        const SPIN_MAX: u64 = 5;
        backoff(
            SPIN_MAX,
            || match self.queue.try_lock() {
                Some(mut queue) => Some(queue
                    .pop()
                    .ok_or(TryRecvError::Empty)),
                None if self.is_disconnected() => Some(Err(TryRecvError::Disconnected)),
                None => None,
            },
            |x| x,
            || { self.wait(|trigger, guard|{ let _ = trigger.wait(guard).unwrap(); }); },
        )
    }

    fn recv(&self) -> Result<T, RecvError> {
        const SPIN_MAX: u64 = 5;
        backoff(
            SPIN_MAX,
            || match self.queue
                .try_lock()?
                .pop()
            {
                Some(msg) => Some(Ok(msg)),
                None if self.is_disconnected() => Some(Err(RecvError::Disconnected)),
                None => None,
            },
            |x| x,
            || { self.wait(|trigger, guard|{ let _ = trigger.wait(guard).unwrap(); }); },
        )
    }

    fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        if let Ok(msg) = self.try_recv() {
            return Ok(msg);
        }

        loop {
            let now = Instant::now();
            let timeout = if now >= deadline {
                return Err(RecvTimeoutError::Timeout);
            } else {
                deadline.duration_since(now)
            };

            if self.wait(|trigger, guard| trigger.wait_timeout(guard, timeout).unwrap().1.timed_out())
                .ok_or(RecvTimeoutError::Disconnected)?
            {
                return Err(RecvTimeoutError::Timeout);
            }

            match self.try_recv() {
                Ok(msg) => return Ok(msg),
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            }
        }
    }
}

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
        self.shared.listen_mode.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Iter<'a, T: Send + 'static> {
    shared: &'a Shared<T>,
}

impl<'a, T: Send + 'static> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.shared.recv().ok()
    }
}

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
        recv_lock: Mutex::new(()),
        send_trigger: Condvar::new(),
        is_disconnected: AtomicBool::new(false),
        senders: AtomicUsize::new(1),
        listen_mode: AtomicUsize::new(1),
    });
    (
        Sender { shared: shared.clone() },
        Receiver { shared },
    )
}
