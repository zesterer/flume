use super::*;

pub(crate) struct Bounded<T> {
    disconnected: bool,
    cap: usize,
    sends: VecDeque<(Waker, Booth<T>)>,
    queue: VecDeque<T>,
    recvs: VecDeque<(Waker, Booth<T>)>
}

impl<T> Bounded<T> {
    pub fn new(cap: usize) -> Flavor<T> {
        Flavor::Bounded(Self {
            disconnected: false,
            cap,
            sends: VecDeque::new(),
            queue: VecDeque::new(),
            recvs: VecDeque::new(),
        })
    }

    pub fn disconnect(&mut self) {
        self.disconnected = true;
        self.sends.drain(..).for_each(|(w, _)| w.wake());
        self.recvs.drain(..).for_each(|(w, _)| w.wake());
    }

    pub fn try_send(&mut self, mut item: T) -> Result<Option<Waker>, (T, bool)> {
        if self.disconnected {
            return Err((item, true));
        }

        loop {
            item = match self.recvs.pop_front() {
                Some((waker, booth)) => match booth.give(item) {
                    Some(item) => item,
                    None => break Ok(Some(waker)), // Woke receiver
                },
                None => break if self.queue.len() < self.cap {
                    self.queue.push_back(item);
                    Ok(None) // Pushed to queue
                } else {
                    Err((item, false)) // Queue is full
                },
            };
        }
    }

    pub fn try_recv(&mut self) -> Result<(Option<Waker>, T), bool> {
        self.queue
            .pop_front()
            .map(|item| {
                // We just made some space in the queue so we need to pull the next waiting sender too
                let w = loop {
                    if let Some((waker, item)) = self.sends.pop_front() {
                        // Attempt to take the item in the slot. If the item does not exist, it must be because the sender
                        // cancelled sending (perhaps due to a timeout). In such a case, don't bother to wake the sender (because
                        // it should already have been woken by the timeout alarm) and skip to the next entry.
                        if let Some(item) = item.try_take() {
                            self.queue.push_back(item);
                            break Some(waker);
                        }
                    } else {
                        break None;
                    }
                };
                (w, item)
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
            .ok_or(self.disconnected)
    }

    pub fn send(&mut self, item: T, waker: impl FnOnce() -> Waker) -> Result<Option<Waker>, Result<Booth<T>, T>> {
        self.try_send(item)
            .map_err(|(item, d)| if d {
                Err(item)
            } else {
                let item = Booth::full(item);
                self.sends.push_back((waker(), item.clone()));
                Ok(item)
            })
    }

    pub fn recv(&mut self, waker: impl FnOnce() -> Waker) -> Result<(Option<Waker>, T), Option<Booth<T>>> {
        self.try_recv()
            .map_err(|d| if d {
                None
            } else {
                let item = Booth::empty();
                self.recvs.push_back((waker(), item.clone()));
                Some(item)
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
