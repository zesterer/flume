use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::{Duration, Instant},
};
use std::sync::{Condvar, Mutex, MutexGuard};

pub trait Msg: Send + 'static {}
impl<T: Send + 'static> Msg for T {}

#[derive(Copy, Clone, Debug)]
pub enum SendError<T: Msg> {
    Disconnected(T),
}

#[derive(Copy, Clone, Debug)]
pub enum RecvError {
    Empty,
    Disconnected,
}

struct Queue<T: Msg> {
    inner: VecDeque<T>,
}

struct Shared<T: Msg> {
    queue: spin::Mutex<Queue<T>>,
    disconnected: Mutex<bool>,
    trigger: Condvar,
    senders: AtomicUsize,
    listen_mode: AtomicUsize,
}

impl<T: Msg> Shared<T> {
    #[inline(always)]
    fn send(&self, msg: T) -> Result<(), SendError<T>> {
        let mut queue = self.queue.lock();

        match self.listen_mode.load(Ordering::Relaxed) {
            2 => self.trigger.notify_all(),
            1 => {},
            0 => return Err(SendError::Disconnected(msg)),
            _ => unreachable!(),
        }

        queue.inner.push_back(msg);

        Ok(())
    }

    #[inline(always)]
    fn all_senders_disconnected(&self) {
        *self.disconnected.lock().unwrap() = true;
        self.trigger.notify_all();
    }

    #[inline(always)]
    fn wait(&self, f: impl FnOnce(&Condvar, MutexGuard<bool>)) {
        self.listen_mode.fetch_add(1, Ordering::Relaxed);
        {
            let disconnected = self.disconnected.lock().unwrap();

            if !*disconnected {
                f(&self.trigger, disconnected);
            }
        }
        self.listen_mode.fetch_sub(1, Ordering::Relaxed);
    }

    #[inline(always)]
    fn try_recv(&self) -> Result<T, RecvError> {
        match self.queue.lock().inner.pop_front() {
            Some(msg) => Ok(msg),
            None if *self.disconnected.lock().unwrap() => Err(RecvError::Disconnected),
            None => Err(RecvError::Empty),
        }
    }

    #[inline(always)]
    fn try_recv_all(&self) -> Result<VecDeque<T>, RecvError> {
        let disconnected = *self.disconnected.lock().unwrap();

        let msgs = {
            let mut msgs = VecDeque::new();
            std::mem::swap(&mut msgs, &mut self.queue.lock().inner);
            msgs
        };

        if msgs.len() == 0 {
            if disconnected {
                Err(RecvError::Disconnected)
            } else {
                Err(RecvError::Empty)
            }
        } else {
            Ok(msgs)
        }
    }

    #[inline(always)]
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

pub struct Sender<T: Msg> {
    shared: Arc<Shared<T>>,
}

impl<T: Msg> Sender<T> {
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.shared.send(msg)
    }
}

impl<T: Msg> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T: Msg> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.all_senders_disconnected();
        }
    }
}

pub struct Receiver<T: Msg> {
    shared: Arc<Shared<T>>,
}

const SPIN_DEFAULT: u64 = 1;
const SPIN_MAX: u64 = 4;

impl<T: Msg> Receiver<T> {
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
            ready: VecDeque::new(),
            spin_time: SPIN_DEFAULT,
        }
    }

    pub fn try_iter(&self) -> impl Iterator<Item=T> + '_ {
        TryIter {
            shared: &self.shared,
            ready: VecDeque::new(),
        }
    }
}

impl<T: Msg> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.listen_mode.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Iter<'a, T: Msg> {
    shared: &'a Shared<T>,
    ready: VecDeque<T>,
    spin_time: u64,
}

impl<'a, T: Msg> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        while self.ready.len() == 0 {
            self.ready = match self.shared.try_recv_all() {
                Ok(msgs) => msgs,
                Err(RecvError::Empty) => {
                    if self.spin_time > SPIN_MAX {
                        self.shared.wait(|trigger, guard| {
                            let _ = trigger.wait(guard).unwrap();
                        });
                    } else {
                        spin_sleep::sleep(Duration::from_nanos(1 << self.spin_time));
                        self.spin_time += 1;
                    }
                    continue
                },
                Err(RecvError::Disconnected) => break,
            };
        }

        self.spin_time = SPIN_DEFAULT;

        let msg = self.ready.pop_front()?;

        Some(msg)
    }
}

pub struct TryIter<'a, T: Msg> {
    shared: &'a Shared<T>,
    ready: VecDeque<T>,
}

impl<'a, T: Msg> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ready.len() == 0 {
            self.ready = match self.shared.try_recv_all() {
                Ok(msgs) => msgs,
                Err(RecvError::Empty) | Err(RecvError::Disconnected) => VecDeque::new(),
            };
        }

        self.ready.pop_front()
    }
}

pub fn channel<T: Msg>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        queue: spin::Mutex::new(Queue {
            inner: VecDeque::new(),
        }),
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
