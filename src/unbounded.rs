use super::*;

pub(crate) struct Unbounded<T> {
    disconnected: bool,
    queue: VecDeque<T>,
    recvs: VecDeque<(Waker, Booth<T>)>
}

impl<T> Unbounded<T> {
    pub fn new() -> Flavor<T> {
        Flavor::Unbounded(Self {
            disconnected: false,
            queue: VecDeque::new(),
            recvs: VecDeque::new(),
        })
    }

    pub fn disconnect(&mut self) {
        self.disconnected = true;
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
                None => {
                    self.queue.push_back(item);
                    break Ok(None); // Pushed to queue
                },
            };
        }
    }

    pub fn try_recv(&mut self) -> Result<T, bool> {
        self.queue.pop_front().ok_or(self.disconnected)
    }

    pub fn send(&mut self, item: T) -> Result<Option<Waker>, (T, bool)> {
        self.try_send(item)
    }

    pub fn recv(&mut self, waker: impl FnOnce() -> Waker) -> Result<T, Option<Booth<T>>> {
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
        core::mem::take(&mut self.queue)
    }

    pub fn len_cap(&self) -> (usize, Option<usize>) {
        (self.queue.len(), None)
    }
}
