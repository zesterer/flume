use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::Duration,
};
use std::sync::{Condvar, Mutex};

pub trait Msg: Send + 'static {}
impl<T: Send + 'static> Msg for T {}

#[derive(Copy, Clone, Debug)]
pub enum SendError {
    Disconnected,
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
    fn send(&self, msg: T) -> Result<(), SendError> {
        self.queue.lock().inner.push_back(msg);

        match self.listen_mode.load(Ordering::Relaxed) {
            2 => self.trigger.notify_all(),
            1 => {},
            0 => return Err(SendError::Disconnected),
            _ => unreachable!(),
        }

        Ok(())
    }

    fn disconnect(&self) {
        *self.disconnected.lock().unwrap() = true;
        self.trigger.notify_all();
    }

    fn wait(&self) {
        self.listen_mode.fetch_add(1, Ordering::Relaxed);
        {
            let disconnected = self.disconnected.lock().unwrap();

            if !*disconnected {
                let _ = self.trigger.wait(disconnected).unwrap();
            }
        }
        self.listen_mode.fetch_sub(1, Ordering::Relaxed);
    }

    fn try_recv(&self) -> Result<T, RecvError> {
        match self.queue.lock().inner.pop_front() {
            Some(msg) => Ok(msg),
            None if *self.disconnected.lock().unwrap() => Err(RecvError::Disconnected),
            None => Err(RecvError::Empty),
        }
    }

    fn try_recv_all(&self) -> Result<VecDeque<T>, RecvError> {
        let disconnected = *self.disconnected.lock().unwrap();

        let msgs = {
            let mut msgs = VecDeque::with_capacity(256);
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

    fn recv(&self) -> Result<T, RecvError> {
        loop {
            match self.try_recv() {
                Ok(msg) => return Ok(msg),
                Err(RecvError::Empty) => {},
                Err(err) => return Err(err),
            }

            self.wait();
        }
    }
}

pub struct Sender<T: Msg> {
    shared: Arc<Shared<T>>,
}

impl<T: Msg> Sender<T> {
    pub fn send(&self, msg: T) -> Result<(), SendError> {
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
            self.shared.disconnect();
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
        self.shared.recv()
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

static mut ELAPSED: usize = 0;
static mut SAVED: usize = 0;

impl<'a, T: Msg> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        while self.ready.len() == 0 {
            self.ready = match self.shared.try_recv_all() {
                Ok(msgs) => msgs,
                Err(RecvError::Empty) => {
                    if self.spin_time > SPIN_MAX {
                        self.shared.wait();
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
            inner: VecDeque::with_capacity(256),
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
