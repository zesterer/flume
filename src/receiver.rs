use super::*;

pub struct Receiver<T>(pub(crate) Arc<Channel<T>>);

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.0.recvs.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.0.recvs.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.disconnect();
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, ()> {
        self.0.try_recv().ok_or(())
    }

    pub fn recv_async(&self) -> RecvFut<T> {
        Channel::recv_async(Cow::Borrowed(&self.0))
    }

    #[cfg(feature = "sync")]
    pub fn recv(&self) -> Result<T, ()> {
        if let Some(item) = self.0.try_recv() {
            Ok(item)
        } else {
            pollster::block_on(self.recv_async())
        }
    }

    #[cfg(feature = "timeout")]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, ()> {
        use futures_timer::Delay;
        use futures_util::future::{select, Either};

        match pollster::block_on(select(
            Delay::new(timeout),
            Channel::recv_async(Cow::Borrowed(&self.0)),
        )) {
            Either::Left((_, _)) => Err(()),
            Either::Right((res, _)) => res,
        }
    }

    #[cfg(feature = "timeout")]
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, ()> {
        self.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    }
}
