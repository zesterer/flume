use super::*;

pub(crate) struct Unbounded<T> {
    queue: VecDeque<T>,
    recvs: VecDeque<(Waker, Booth<T>)>
}

impl<T> Unbounded<T> {
    pub fn new() -> Flavor<T> {
        Flavor::Unbounded(Lock::new(Self {
            queue: VecDeque::new(),
            recvs: VecDeque::new(),
        }))
    }

    pub fn disconnect(&mut self) {
        self.recvs.drain(..).for_each(|(w, _)| w.wake());
    }

    pub fn try_send(&mut self, item: T) -> Option<Waker> {
        match self.recvs.pop_front() {
            Some((waker, booth)) => {
                booth.give(item);
                Some(waker)
            },
            None => {
                self.queue.push_back(item);
                None
            },
        }
    }

    pub fn try_recv(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    pub fn send(&mut self, item: T) -> Option<Waker> {
        self.try_send(item)
    }

    pub fn recv(&mut self, waker: impl FnOnce() -> Waker) -> Result<T, Booth<T>> {
        self.try_recv()
            .ok_or_else(|| {
                let item = Booth::empty();
                self.recvs.push_back((waker(), item.clone()));
                item
            })
    }
}
