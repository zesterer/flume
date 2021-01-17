use super::*;

pub struct Sender<T>(pub(crate) Arc<Channel<T>>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.sends.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.0.sends.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.disconnect();
        }
    }
}

impl<T> Sender<T> {
    pub fn try_send(&self, item: T) -> Result<(), T> {
        self.0.try_send(item).map(Err).unwrap_or(Ok(()))
    }

    pub fn send_async(&self, item: T) -> SendFut<T> {
        Channel::send_async(Cow::Borrowed(&self.0), item)
    }

    #[cfg(feature = "sync")]
    pub fn send(&self, item: T) -> Result<(), T> {
        if let Some(item) = self.0.try_send(item) {
            pollster::block_on(self.send_async(item))
        } else {
            Ok(())
        }
    }

    #[cfg(feature = "timeout")]
    pub fn send_timeout(&self, item: T, timeout: Duration) -> Result<(), T> {
        use futures_timer::Delay;
        use futures_util::future::{select, Either};

        match pollster::block_on(select(
            Delay::new(timeout),
            Channel::send_async(Cow::Borrowed(&self.0), item),
        )) {
            Either::Left((_, send_fut)) => send_fut.try_reclaim().map(Err).unwrap_or(Ok(())),
            Either::Right((res, _)) => res,
        }
    }

    #[cfg(feature = "timeout")]
    pub fn send_deadline(&self, item: T, deadline: Instant) -> Result<(), T> {
        self.send_timeout(item, deadline.saturating_duration_since(Instant::now()))
    }
}
