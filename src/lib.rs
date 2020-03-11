use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::{Duration, Instant},
};
use std::sync::{Condvar, Mutex, MutexGuard};

#[derive(Copy, Clone, Debug)]
pub struct SendError<T: Send + 'static>(T);

#[derive(Copy, Clone, Debug)]
pub enum RecvError {
    Empty,
    Disconnected,
}

#[derive(Copy, Clone, Debug)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

#[derive(Copy, Clone, Debug)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

struct Shared<T: Send + 'static> {
    queue: spin::Mutex<VecDeque<T>>,
    disconnected: Mutex<bool>,
    trigger: Condvar,
    senders: AtomicUsize,
    listen_mode: AtomicUsize,
}

fn backoff<T, U>(
    spin_max: u64,
    mut f: impl FnMut() -> Option<T>,
    ok: impl FnOnce(T) -> U,
    mut wait: impl FnMut(),
) -> U {
    let mut spin_time = 1;
    loop {
        match f() {
            Some(r) => break ok(r),
            None => if spin_time > spin_max {
                wait();
            } else {
                spin_sleep::sleep(Duration::from_nanos(1 << spin_time));
                spin_time += 1;
            },
        }
    }
}

impl<T: Send + 'static> Shared<T> {
    fn send(&self, msg: T) -> Result<(), SendError<T>> {
        backoff(
            !0,
            || self.queue.try_lock(),
            |mut queue| {
                match self.listen_mode.load(Ordering::Relaxed) {
                    0 => return Err(SendError(msg)),
                    1 => {},
                    _ => self.trigger.notify_all(),
                }

                queue.push_back(msg);
                Ok(())
            },
            || {},
        )
    }

    fn all_senders_disconnected(&self) {
        *self.disconnected.lock().unwrap() = true;
        self.trigger.notify_all();
    }

    fn wait<R>(&self, f: impl FnOnce(&Condvar, MutexGuard<bool>) -> R) -> Option<R> {
        self.listen_mode.fetch_add(1, Ordering::Acquire);
        let r = {
            let disconnected = self.disconnected.lock().unwrap();

            if !*disconnected {
                Some(f(&self.trigger, disconnected))
            } else {
                None
            }
        };
        self.listen_mode.fetch_sub(1, Ordering::Release);
        r
    }

    fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.queue
            .try_lock()
            .ok_or(TryRecvError::Empty)?
            .pop_front()
        {
            Some(msg) => Ok(msg),
            None if *self.disconnected.lock().unwrap() => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    fn recv(&self) -> Result<T, RecvError> {
        const SPIN_MAX: u64 = 5;
        backoff(
            SPIN_MAX,
            || match self.try_recv() {
                Ok(msg) => Some(Ok(msg)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(RecvError::Disconnected)),
            },
            |x| x,
            || { self.wait(|trigger, guard|{ let _ = trigger.wait(guard).unwrap(); }); },
        )
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        loop {
            match self.try_recv() {
                Ok(msg) => return Ok(msg),
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            }

            if self.wait(|trigger, guard| trigger.wait_timeout(guard, timeout).unwrap().1.timed_out())
                .ok_or(RecvTimeoutError::Disconnected)?
            {
                return Err(RecvTimeoutError::Timeout);
            }

            // TODO: Don't reset timeout!
        }
    }
}

pub struct Sender<T: Send + 'static> {
    shared: Arc<Shared<T>>,
}

impl<T: Send + 'static> Sender<T> {
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
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.shared.recv_timeout(timeout)
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_timeout(deadline.duration_since(Instant::now()))
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.shared.try_recv()
    }

    pub fn iter(&self) -> impl Iterator<Item=T> + '_ {
        Iter { shared: &self.shared }
    }

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

pub fn channel<T: Send + 'static>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        queue: spin::Mutex::new(VecDeque::new()),
        disconnected: Mutex::new(false),
        trigger: Condvar::new(),
        senders: AtomicUsize::new(1),
        listen_mode: AtomicUsize::new(1),
    });
    (
        Sender { shared: shared.clone() },
        Receiver { shared },
    )
}
