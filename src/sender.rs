use super::*;

pub struct Sender<T>(pub(crate) Arc<Channel<T>>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan().sends.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.chan().sends.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.chan().disconnect();
        }
    }
}

impl<T> Sender<T> {
    pub(crate) fn chan(&self) -> &Channel<T> {
        &self.0
    }

    pub fn try_send(&self, item: T) -> Result<(), T> {
        self.chan().try_send(item).map(Err).unwrap_or(Ok(()))
    }

    pub fn send_async(&self, item: T) -> SendFut<T> {
        Channel::send_async(Cow::Borrowed(&self), item)
    }

    pub fn into_send_async(self, item: T) -> IntoSendFut<T> {
        Channel::send_async(Cow::Owned(self), item)
    }

    #[cfg(feature = "sync")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
    pub fn send(&self, item: T) -> Result<(), T> {
        if let Some(item) = self.chan().try_send(item) {
            pollster::block_on(self.send_async(item))
        } else {
            Ok(())
        }
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub async fn send_timeout_async(&self, item: T, timeout: Duration) -> Result<(), T> {
        use futures_timer::Delay;
        use futures_util::future::{select, Either, FutureExt};

        // Speculatively perform a send to potentially avoid unnecessarily setting up timing things
        let item = match self.try_send(item) {
            Ok(()) => return Ok(()),
            Err(item) => item,
        };

        select(
            Delay::new(timeout),
            self.send_async(item),
        ).map(|res| match res {
            Either::Left((_, send_fut)) => send_fut.try_reclaim().map(Err).unwrap_or(Ok(())),
            Either::Right((res, _)) => res,
        }).await
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub async fn send_deadline_async(&self, item: T, deadline: Instant) -> Result<(), T> {
        self.send_timeout_async(item, deadline.saturating_duration_since(Instant::now())).await
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn send_timeout(&self, item: T, timeout: Duration) -> Result<(), T> {
        pollster::block_on(self.send_timeout_async(item, timeout))
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn send_deadline(&self, item: T, deadline: Instant) -> Result<(), T> {
        pollster::block_on(self.send_deadline_async(item, deadline))
    }

    #[cfg(feature = "sink")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sink")))]
    pub fn sink(&self) -> SendSink<T> {
        SendSink { send: Some(Channel::send_async_exhausted(Cow::Borrowed(&self))) }
    }

    #[cfg(feature = "sink")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sink")))]
    pub fn into_sink(self) -> IntoSendSink<T> {
        IntoSendSink { send: Some(Channel::send_async_exhausted(Cow::Owned(self))) }
    }

    pub fn is_disconnected(&self) -> bool { self.chan().is_disconnected() }

    pub fn is_empty(&self) -> bool { self.chan().is_empty() }

    pub fn is_full(&self) -> bool { self.chan().is_full() }

    pub fn len(&self) -> usize { self.chan().len_cap().0 }

    pub fn capacity(&self) -> Option<usize> { self.chan().len_cap().1 }
}

pub type IntoSendFut<T> = SendFut<'static, T>;

#[cfg(feature = "sink")]
#[cfg_attr(docsrs, doc(cfg(feature = "sink")))]
pub type IntoSendSink<T> = SendSink<'static, T>;
