use super::*;

pub(crate) struct Bounded<T> {
    cap: usize,
    sends: VecDeque<(Waker, Booth<T>)>,
    queue: VecDeque<T>,
    recvs: VecDeque<(Waker, Booth<T>)>
}

impl<T> Bounded<T> {
    pub fn new(cap: usize) -> Flavor<T> {
        Flavor::Bounded(Lock::new(Self {
            cap,
            sends: VecDeque::new(),
            queue: VecDeque::new(),
            recvs: VecDeque::new(),
        }))
    }

    pub fn disconnect(&mut self) {
        self.sends.drain(..).for_each(|(w, _)| w.wake());
        self.recvs.drain(..).for_each(|(w, _)| w.wake());
    }

    pub fn try_send(&mut self, item: T) -> Result<Option<Waker>, T> {
        match self.recvs.pop_front() {
            Some((waker, booth)) => {
                booth.give(item);
                Ok(Some(waker))
            },
            None => if self.queue.len() < self.cap {
                self.queue.push_back(item);
                Ok(None)
            } else {
                Err(item)
            },
        }
    }

    pub fn try_recv(&mut self) -> Option<(Option<Waker>, T)> {
        self.queue
            .pop_front()
            .map(|item| {
                // We just made some space in the queue so we need to pull the next waiting sender too
                loop {
                    if let Some((waker, item)) = self.sends.pop_front() {
                        // Attempt to take the item in the slot. If the item does not exist, it must be because the sender
                        // cancelled sending (perhaps due to a timeout). In such a case, don't bother to wake the sender (because
                        // it should already have been woken by the timeout alarm) and skip to the next entry.
                        if let Some(item) = item.try_take() {
                            break (Some(waker), item);
                        }
                    } else {
                        break (None, item);
                    }
                }
            })
            .or_else(|| {
                while let Some((waker, item)) = self.sends.pop_front() {
                    // Attempt to take the item in the slot. If the item does not exist, it must be because the sender
                    // cancelled sending (perhaps due to a timeout). In such a case, don't bother to wake the sender (because
                    // it should already have been woken by the timeout alarm) and skip to the next entry.
                    if let Some(item) = item.try_take() {
                        return Some((Some(waker), item));
                    }
                }
                None
            })
    }

    pub fn send(&mut self, item: T, waker: impl FnOnce() -> Waker) -> Result<Option<Waker>, Booth<T>> {
        self.try_send(item)
            .map_err(|item| {
                let item = Booth::full(item);
                self.sends.push_back((waker(), item.clone()));
                item
            })
    }

    pub fn recv(&mut self, waker: impl FnOnce() -> Waker) -> Result<(Option<Waker>, T), Booth<T>> {
        self.try_recv()
            .ok_or_else(|| {
                let item = Booth::empty();
                self.recvs.push_back((waker(), item.clone()));
                item
            })
    }

    pub fn drain(&mut self) -> VecDeque<T> {
        let items = core::mem::take(&mut self.queue);

        // We've made space in the queue so we need to pull waiting senders
        while self.queue.len() < self.cap {
            if let Some((waker, item)) = self.sends.pop_front() {
                // Attempt to take the item in the slot. If the item does not exist, it must be because the sender
                // cancelled sending (perhaps due to a timeout). In such a case, don't bother to wake the sender (because
                // it should already have been woken by the timeout alarm) and skip to the next entry.
                if let Some(item) = item.try_take() {
                    waker.wake();
                    self.queue.push_back(item);
                }
            } else {
                break;
            }
        }

        items
    }

    pub fn len_cap(&self) -> (usize, Option<usize>) {
        (self.queue.len(), Some(self.cap))
    }
}
