use super::*;

pub(crate) struct Rendezvous<T> {
    sends: VecDeque<(Waker, Booth<T>)>,
    recvs: VecDeque<(Waker, Booth<T>)>
}

impl<T> Rendezvous<T> {
    pub fn new() -> Flavor<T> {
        Flavor::Rendezvous(Lock::new(Self {
            sends: VecDeque::new(),
            recvs: VecDeque::new(),
        }))
    }

    pub fn disconnect(&mut self) {
        self.sends.drain(..).for_each(|(w, _)| w.wake());
        self.recvs.drain(..).for_each(|(w, _)| w.wake());
    }

    pub fn try_send(&mut self, item: T) -> Result<Waker, T> {
        match self.recvs.pop_front() {
            Some((waker, booth)) => {
                booth.give(item);
                Ok(waker)
            },
            None => Err(item),
        }
    }

    pub fn try_recv(&mut self) -> Option<(Waker, T)> {
        while let Some((waker, item)) = self.sends.pop_front() {
            // Attempt to take the item in the slot. If the item does not exist, it must be because the sender
            // cancelled sending (perhaps due to a timeout). In such a case, don't bother to wake the sender (because
            // it should already have been woken by the timeout alarm) and skip to the next entry.
            if let Some(item) = item.try_take() {
                return Some((waker, item));
            }
        }
        None
    }

    pub fn send(&mut self, item: T, waker: impl FnOnce() -> Waker) -> Result<Waker, Booth<T>> {
        self.try_send(item)
            .map_err(|item| {
                let item = Booth::full(item);
                self.sends.push_back((waker(), item.clone()));
                item
            })
    }

    pub fn recv(&mut self, waker: impl FnOnce() -> Waker) -> Result<(Waker, T), Booth<T>> {
        self.try_recv()
            .ok_or_else(|| {
                let item = Booth::empty();
                self.recvs.push_back((waker(), item.clone()));
                item
            })
    }
}
