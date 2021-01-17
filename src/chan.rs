use super::*;

pub(crate) enum Flavor<T> {
    Rendezvous(Lock<Rendezvous<T>>),
    Bounded(Lock<Bounded<T>>),
    Unbounded(Lock<Unbounded<T>>),
}

pub(crate) struct Channel<T> {
    flavor: Flavor<T>,
    disconnected: AtomicBool,
    pub sends: AtomicUsize,
    pub recvs: AtomicUsize,
}

impl<T> Channel<T> {
    pub fn new(flavor: Flavor<T>) -> Self {
        Self {
            flavor,
            disconnected: AtomicBool::new(false),
            sends: AtomicUsize::new(1),
            recvs: AtomicUsize::new(1),
        }
    }

    pub fn disconnect(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
        match &self.flavor {
            Flavor::Rendezvous(f) => wait_lock(f).disconnect(),
            Flavor::Bounded(f) => wait_lock(f).disconnect(),
            Flavor::Unbounded(f) => wait_lock(f).disconnect(),
        }
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Relaxed)
    }

    pub fn try_send(&self, item: T) -> Option<T> {
        match &self.flavor {
            Flavor::Rendezvous(f) => wait_lock(f).try_send(item).map(|w| w.wake()).err(),
            Flavor::Bounded(f) => wait_lock(f).try_send(item).map(|w| w.map(|w| w.wake())).err(),
            Flavor::Unbounded(f) => { wait_lock(f).try_send(item).map(|w| w.wake()); None },
        }
    }

    pub fn try_recv(&self) -> Option<T> {
        match &self.flavor {
            Flavor::Rendezvous(f) => wait_lock(f).try_recv().map(|(waker, item)| { waker.wake(); item }),
            Flavor::Bounded(f) => wait_lock(f).try_recv().map(|(waker, item)| { waker.map(|w| w.wake()); item }),
            Flavor::Unbounded(f) => wait_lock(f).try_recv(),
        }
    }

    pub fn send_async<'a>(chan: Cow<'a, Arc<Self>>, item: T) -> SendFut<'a, T> {
        SendFut { chan, item: Some(Err(item)) }
    }

    pub fn recv_async<'a>(chan: Cow<'a, Arc<Self>>) -> RecvFut<'a, T> {
        RecvFut { chan, item: Some(None) }
    }
}

#[pin_project]
pub struct SendFut<'a, T> {
    chan: Cow<'a, Arc<Channel<T>>>,
    item: Option<Result<Booth<T>, T>>,
}

impl<'a, T> SendFut<'a, T> {
    #[cfg(feature = "timeouts")]
    pub(crate) fn try_reclaim(mut self) -> Option<T> {
        self.item
            .take()
            .and_then(|item| match item {
                Ok(item) => item.try_take(),
                Err(item) => Some(item),
            })
    }
}

impl<'a, T> Future for SendFut<'a, T> {
    type Output = Result<(), T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        *this.item = if let Some(item) = this.item.take() {
            Some(match item {
                Err(item) => match match &this.chan.flavor {
                    Flavor::Rendezvous(f) => wait_lock(f).send(item, || cx.waker().clone()).map(|w| w.wake()),
                    Flavor::Bounded(f) => wait_lock(f).send(item, || cx.waker().clone()).map(|w| { w.map(|w| w.wake()); }),
                    Flavor::Unbounded(f) => Ok(wait_lock(f).send(item).map(|w| w.wake()).unwrap_or(())),
                } {
                    Ok(()) => return Poll::Ready(Ok(())),
                    Err(item) => Ok(item),
                },
                Ok(item) => if !item.is_full() {
                    return Poll::Ready(Ok(()));
                } else if this.chan.is_disconnected() {
                    return Poll::Ready(item.try_take().map(Err).unwrap_or(Ok(())));
                } else {
                    Ok(item)
                },
            })
        } else {
            // We got polled *again*?
            return Poll::Pending;
        };

        Poll::Pending
    }
}

#[pin_project]
pub struct RecvFut<'a, T> {
    chan: Cow<'a, Arc<Channel<T>>>,
    item: Option<Option<Booth<T>>>,
}

impl<'a, T> Future for RecvFut<'a, T> {
    type Output = Result<T, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        *this.item = if let Some(item) = this.item.take() {
            Some(match item {
                None => match match &this.chan.flavor {
                    Flavor::Rendezvous(f) => wait_lock(f).recv(|| cx.waker().clone()).map(|(w, item)| { w.wake(); item }),
                    Flavor::Bounded(f) => wait_lock(f).recv(|| cx.waker().clone()).map(|(w, item)| { w.map(|w| w.wake()); item }),
                    Flavor::Unbounded(f) => wait_lock(f).recv(|| cx.waker().clone()),
                } {
                    Ok(item) => return Poll::Ready(Ok(item)),
                    Err(item) => Some(item),
                },
                Some(item) => if let Some(item) = item.try_take() {
                    return Poll::Ready(Ok(item));
                } else if this.chan.is_disconnected() {
                    return Poll::Ready(Err(()));
                } else {
                    Some(item)
                },
            })
        } else {
            // We got polled *again*?
            return Poll::Pending;
        };

        Poll::Pending
    }
}

#[derive(Default)]
pub(crate) struct Booth<T> {
    inner: Arc<Lock<Option<T>>>,
}

impl<T> Clone for Booth<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<T> Booth<T> {
    pub fn full(item: T) -> Self {
        Self { inner: Arc::new(Lock::new(Some(item))), }
    }

    pub fn empty() -> Self {
        Self { inner: Arc::new(Lock::new(None)), }
    }

    pub fn give(self, item: T) {
        let mut guard = self.inner.lock();
        debug_assert!(guard.is_none());
        *guard = Some(item);
    }

    pub fn try_take(&self) -> Option<T> {
        self.inner.lock().take()
    }

    pub fn is_full(&self) -> bool {
        self.inner.lock().is_some()
    }
}
