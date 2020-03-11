use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::{Duration, Instant},
};
use std::sync::{Condvar, Mutex, MutexGuard};

#[derive(Copy, Clone, Debug)]
pub enum SendError<T: Send + 'static> {
    Disconnected(T),
}

#[derive(Copy, Clone, Debug)]
pub enum RecvError {
    Empty,
    Disconnected,
}

struct Shared<T: Send + 'static> {
    queue: spin::Mutex<VecDeque<T>>,
    disconnected: Mutex<bool>,
    trigger: Condvar,
    senders: AtomicUsize,
    listen_mode: AtomicUsize,
}

impl<T: Send + 'static> Shared<T> {
    fn send(&self, msg: T) -> Result<(), SendError<T>> {
        let mut queue = self.queue.lock();

        match self.listen_mode.load(Ordering::Relaxed) {
            0 => return Err(SendError::Disconnected(msg)),
            1 => {},
            _ => self.trigger.notify_all(),
        }

        queue.push_back(msg);

        Ok(())
    }

    fn all_senders_disconnected(&self) {
        *self.disconnected.lock().unwrap() = true;
        self.trigger.notify_all();
    }

    fn wait(&self, f: impl FnOnce(&Condvar, MutexGuard<bool>)) {
        self.listen_mode.fetch_add(1, Ordering::Acquire);
        {
            let disconnected = self.disconnected.lock().unwrap();

            if !*disconnected {
                f(&self.trigger, disconnected);
            }
        }
        self.listen_mode.fetch_sub(1, Ordering::Release);
    }

    fn try_recv(&self) -> Result<T, RecvError> {
        match self.queue.lock().pop_front() {
            Some(msg) => Ok(msg),
            None if *self.disconnected.lock().unwrap() => Err(RecvError::Disconnected),
            None => Err(RecvError::Empty),
        }
    }

    fn recv(&self, timeout: Option<Duration>) -> Result<T, RecvError> {
        loop {
            match self.try_recv() {
                Ok(msg) => return Ok(msg),
                Err(RecvError::Empty) if timeout.is_none() => {},
                Err(err) => return Err(err),
            }

            self.wait(|trigger, guard| {
                let _ = match timeout {
                    Some(timeout) => trigger.wait_timeout(guard, timeout).unwrap().0,
                    None => trigger.wait(guard).unwrap(),
                };
            });
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

const SPIN_DEFAULT: u64 = 1;
const SPIN_MAX: u64 = 4;

impl<T: Send + 'static> Receiver<T> {
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv(None)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvError> {
        self.shared.recv(Some(timeout))
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvError> {
        self.shared.recv(Some(deadline.duration_since(Instant::now())))
    }

    pub fn try_recv(&self) -> Result<T, RecvError> {
        self.shared.try_recv()
    }

    pub fn iter(&self) -> impl Iterator<Item=T> + '_ {
        Iter {
            shared: &self.shared,
            spin_time: SPIN_DEFAULT,
        }
    }

    pub fn try_iter(&self) -> impl Iterator<Item=T> + '_ {
        TryIter {
            shared: &self.shared,
        }
    }
}

impl<T: Send + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.listen_mode.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Iter<'a, T: Send + 'static> {
    shared: &'a Shared<T>,
    spin_time: u64,
}

impl<'a, T: Send + 'static> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let msg = loop {
            match self.shared.try_recv() {
                Ok(msg) => break Some(msg),
                Err(RecvError::Empty) => if self.spin_time > SPIN_MAX {
                    self.shared.wait(|trigger, guard| {
                        let _ = trigger.wait(guard).unwrap();
                    });
                } else {
                    spin_sleep::sleep(Duration::from_nanos(1 << self.spin_time));
                    self.spin_time += 1;
                },
                Err(RecvError::Disconnected) => break None,
            }
        };

        self.spin_time = SPIN_DEFAULT;

        msg
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
            Err(RecvError::Empty) | Err(RecvError::Disconnected) => None,
        }
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
