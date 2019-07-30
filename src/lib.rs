use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

pub trait Msg: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Msg for T {}

struct Queue<T: Msg> {
    inner: spin::Mutex<VecDeque<T>>,
    lock: Mutex<()>,
    trigger: Condvar,
}

impl<T: Msg> Queue<T> {
    fn send(&self, msg: T) {
        self.inner.lock().push_back(msg);
        self.trigger.notify_one();
    }

    fn wait(&self) {
        let _ = self.trigger.wait(self.lock.lock().unwrap()).unwrap();
    }

    fn recv(&self) -> T {
        loop {
            if let Some(msg) = self.try_recv() {
                return msg;
            }

            self.wait();
        }
    }

    fn try_recv(&self) -> Option<T> {
        self.inner.lock().pop_front()
    }

    fn recv_ready(&self) -> VecDeque<T> {
        let mut tmp = VecDeque::new();
        std::mem::swap(&mut tmp, &mut self.inner.lock());
        tmp
    }
}

#[derive(Clone)]
pub struct Sender<T: Msg> {
    queue: Arc<Queue<T>>,
}

impl<T: Msg> Sender<T> {
    pub fn send(&self, msg: T) {
        self.queue.send(msg);
    }
}

pub struct Receiver<T: Msg> {
    queue: Arc<Queue<T>>,
}

impl<T: Msg> Receiver<T> {
    pub fn recv(&self) -> T {
        self.queue.recv()
    }

    pub fn try_recv(&self) -> Option<T> {
        self.queue.try_recv()
    }

    pub fn iter(&self) -> impl Iterator<Item=T> + '_ {
        Iter {
            queue: &self.queue,
            ready: VecDeque::new(),
        }
    }

    pub fn try_iter(&self) -> impl Iterator<Item=T> + '_ {
        TryIter {
            queue: &self.queue,
            ready: VecDeque::new(),
        }
    }
}

pub struct Iter<'a, T: Msg> {
    queue: &'a Queue<T>,
    ready: VecDeque<T>,
}

impl<'a, T: Msg> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.ready.len() == 0 {
                self.ready = self.queue.recv_ready();
            }

            match self.ready.pop_front() {
                Some(msg) => return Some(msg),
                None => self.queue.wait(),
            }
        }
    }
}

pub struct TryIter<'a, T: Msg> {
    queue: &'a Queue<T>,
    ready: VecDeque<T>,
}

impl<'a, T: Msg> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ready.len() == 0 {
            self.ready = self.queue.recv_ready();
        }

        self.ready.pop_front()
    }
}

pub fn channel<T: Msg>() -> (Sender<T>, Receiver<T>) {
    let queue = Arc::new(Queue {
        inner: spin::Mutex::new(VecDeque::new()),
        lock: Mutex::new(()),
        trigger: Condvar::new(),
    });
    (
        Sender { queue: queue.clone() },
        Receiver { queue },
    )
}
