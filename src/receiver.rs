use super::*;

pub struct Receiver<T>(pub(crate) Arc<Channel<T>>);

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.chan().recvs.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.chan().recvs.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.chan().disconnect();
        }
    }
}

impl<T> Receiver<T> {
    pub(crate) fn chan(&self) -> &Channel<T> {
        &self.0
    }

    pub fn try_recv(&self) -> Result<T, ()> {
        self.chan().try_recv().ok_or(())
    }

    pub fn recv_async(&self) -> RecvFut<T> {
        Channel::recv_async(Cow::Borrowed(&self))
    }

    pub fn into_recv_async(self) -> IntoRecvFut<T> {
        Channel::recv_async(Cow::Owned(self))
    }

    #[cfg(feature = "sync")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
    pub fn recv(&self) -> Result<T, ()> {
        if let Some(item) = self.chan().try_recv() {
            Ok(item)
        } else {
            pollster::block_on(self.recv_async())
        }
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub async fn recv_timeout_async(&self, timeout: Duration) -> Result<T, ()> {
        use futures_timer::Delay;
        use futures_util::future::{select, Either, FutureExt};

        // Speculatively perform a recv to potentially avoid unnecessarily setting up timing things
        match self.try_recv() {
            Ok(item) => return Ok(item),
            Err(()) => {},
        }

        select(
            Delay::new(timeout),
            self.recv_async(),
        ).map(|res| match res {
            Either::Left((_, recv_fut)) => recv_fut.try_reclaim().map(Ok).unwrap_or(Err(())),
            Either::Right((res, _)) => res,
        }).await
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub async fn recv_deadline_async(&self, deadline: Instant) -> Result<T, ()> {
        self.recv_timeout_async(deadline.saturating_duration_since(Instant::now())).await
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, ()> {
        pollster::block_on(self.recv_timeout_async(timeout))
    }

    #[cfg(feature = "time")]
    #[cfg_attr(docsrs, doc(cfg(feature = "time")))]
    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, ()> {
        pollster::block_on(self.recv_deadline_async(deadline))
    }

    #[cfg(feature = "sync")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
    pub fn iter(&self) -> Iter<T> {
        Iter { recv: Cow::Borrowed(self) }
    }

    #[cfg(feature = "sync")]
    #[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
    pub fn into_iter(self) -> IntoIter<T> {
        Iter { recv: Cow::Owned(self) }
    }

    pub fn try_iter(&self) -> TryIter<T> {
        TryIter { recv: Cow::Borrowed(self) }
    }

    pub fn into_try_iter(self) -> IntoTryIter<T> {
        TryIter { recv: Cow::Owned(self) }
    }

    #[cfg(feature = "stream")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
    pub fn stream(&self) -> RecvStream<T> {
        RecvStream { recv: Some(Channel::recv_async(Cow::Borrowed(&self))) }
    }

    #[cfg(feature = "stream")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
    pub fn into_stream(self) -> IntoRecvStream<T> {
        IntoRecvStream { recv: Some(Channel::recv_async(Cow::Owned(self))) }
    }

    pub fn drain(&self) -> Drain<T> {
        Drain {
            items: self.chan().drain(),
            phantom: PhantomData,
        }
    }

    pub fn into_drain(self) -> IntoDrain<T> {
        Drain {
            items: self.chan().drain(),
            phantom: PhantomData,
        }
    }

    pub fn is_disconnected(&self) -> bool { self.chan().is_disconnected() }

    pub fn is_empty(&self) -> bool { self.chan().is_empty() }

    pub fn is_full(&self) -> bool { self.chan().is_full() }

    pub fn len(&self) -> usize { self.chan().len_cap().0 }

    pub fn capacity(&self) -> Option<usize> { self.chan().len_cap().1 }
}

pub type IntoRecvFut<T> = RecvFut<'static, T>;

#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
pub type IntoRecvStream<T> = RecvStream<'static, T>;

#[cfg(feature = "sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
pub struct Iter<'a, T> {
    recv: Cow<'a, Receiver<T>>,
}

#[cfg(feature = "sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
pub type IntoIter<T> = Iter<'static, T>;

#[cfg(feature = "sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
impl<'a, T> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv.recv().ok()
    }
}

pub struct TryIter<'a, T> {
    recv: Cow<'a, Receiver<T>>,
}

pub type IntoTryIter<T> = TryIter<'static, T>;

impl<'a, T> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv.try_recv().ok()
    }
}

pub struct Drain<'a, T> {
    items: VecDeque<T>,
    phantom: PhantomData<&'a ()>,
}

pub type IntoDrain<T> = Drain<'static, T>;

impl<'a, T> Iterator for Drain<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.items.pop_front()
    }
}
