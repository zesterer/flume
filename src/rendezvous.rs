use super::*;

pub(crate) struct Rendezvous<T> {
    disconnected: bool,
    sends: VecDeque<(Waker, Booth<T>)>,
    recvs: VecDeque<(Waker, Booth<T>)>
}

impl<T> Rendezvous<T> {
    pub fn new() -> Flavor<T> {
        Flavor::Rendezvous(Self {
            disconnected: false,
            sends: VecDeque::new(),
            recvs: VecDeque::new(),
        })
    }

    pub fn disconnect(&mut self) {
        self.disconnected = true;
        self.sends.drain(..).for_each(|(w, _)| w.wake());
        self.recvs.drain(..).for_each(|(w, _)| w.wake());
    }

    pub fn try_send(&mut self, mut item: T) -> Result<Waker, (T, bool)> {
        if self.disconnected {
            return Err((item, true));
        }

        loop {
            item = match self.recvs.pop_front() {
                Some((waker, booth)) => match booth.give(item) {
                    Some(item) => item,
                    None => break Ok(waker), // Woke receiver
                },
                None => break Err((item, false)), // Queue is full,
            };
        }
    }

    pub fn try_recv(&mut self) -> Result<(Waker, T), bool> {
        while let Some((waker, item)) = self.sends.pop_front() {
            // Attempt to take the item in the slot. If the item does not exist, it must be because the sender
            // cancelled sending (perhaps due to a timeout). In such a case, don't bother to wake the sender (because
            // it should already have been woken by the timeout alarm) and skip to the next entry.
            if let Some(item) = item.try_take() {
                return Ok((waker, item));
            }
        }
        Err(self.disconnected)
    }

    pub fn send(&mut self, item: T, waker: impl FnOnce() -> Waker) -> Result<Waker, Result<Booth<T>, T>> {
        self.try_send(item)
            .map_err(|(item, d)| if d {
                Err(item)
            } else {
                let item = Booth::full(item);
                self.sends.push_back((waker(), item.clone()));
                Ok(item)
            })
    }

    pub fn recv(&mut self, waker: impl FnOnce() -> Waker) -> Result<(Waker, T), Option<Booth<T>>> {
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
        VecDeque::new()
    }

    pub fn len_cap(&self) -> (usize, Option<usize>) {
        (0, Some(0))
    }
}
