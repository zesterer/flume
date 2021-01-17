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

    pub fn len_cap(&self) -> (usize, Option<usize>) {
        match &self.flavor {
            Flavor::Rendezvous(f) => wait_lock(f).len_cap(),
            Flavor::Bounded(f) => wait_lock(f).len_cap(),
            Flavor::Unbounded(f) => wait_lock(f).len_cap(),
        }
    }

    pub fn is_empty(&self) -> bool { self.len_cap().0 == 0 }

    pub fn is_full(&self) -> bool {
        let (len, cap) = self.len_cap();
        cap.map_or(false, |cap| {
            debug_assert!(len <= cap, "This isn't supposed to happen, please submit a bug report.");
            len == cap
        })
    }

    pub fn drain(&self) -> VecDeque<T> {
        match &self.flavor {
            Flavor::Rendezvous(f) => wait_lock(f).drain(),
            Flavor::Bounded(f) => wait_lock(f).drain(),
            Flavor::Unbounded(f) => wait_lock(f).drain(),
        }
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

    pub fn send_async<'a>(sender: Cow<'a, Sender<T>>, item: T) -> SendFut<'a, T> {
        SendFut { sender, item: Some(Err(item)) }
    }

    #[cfg(feature = "sink")]
    pub fn send_async_exhausted<'a>(sender: Cow<'a, Sender<T>>) -> SendFut<'a, T> {
        SendFut { sender, item: None }
    }

    pub fn recv_async<'a>(receiver: Cow<'a, Receiver<T>>) -> RecvFut<'a, T> {
        RecvFut { receiver, item: Some(None) }
    }
}

pin_project! {
    pub struct SendFut<'a, T> {
        sender: Cow<'a, Sender<T>>,
        item: Option<Result<Booth<T>, T>>,
    }
}

impl<'a, T> SendFut<'a, T> {
    #[cfg(feature = "sink")]
    pub(crate) fn into_sender(self) -> Cow<'a, Sender<T>> {
        self.sender
    }

    #[cfg(feature = "time")]
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
                Err(item) => match match &this.sender.0.flavor {
                    Flavor::Rendezvous(f) => wait_lock(f).send(item, || cx.waker().clone()).map(|w| w.wake()),
                    Flavor::Bounded(f) => wait_lock(f).send(item, || cx.waker().clone()).map(|w| { w.map(|w| w.wake()); }),
                    Flavor::Unbounded(f) => Ok(wait_lock(f).send(item).map(|w| w.wake()).unwrap_or(())),
                } {
                    Ok(()) => return Poll::Ready(Ok(())),
                    Err(item) => Ok(item),
                },
                Ok(item) => if !item.is_full() {
                    return Poll::Ready(Ok(()));
                } else if this.sender.0.is_disconnected() {
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

impl<'a, T> FusedFuture for SendFut<'a, T> {
    fn is_terminated(&self) -> bool { self.item.is_none() }
}

pin_project! {
    pub struct RecvFut<'a, T> {
        receiver: Cow<'a, Receiver<T>>,
        item: Option<Option<Booth<T>>>,
    }
}

impl<'a, T> RecvFut<'a, T> {
    #[cfg(feature = "stream")]
    pub(crate) fn into_receiver(self) -> Cow<'a, Receiver<T>> {
        self.receiver
    }

    #[cfg(feature = "time")]
    pub(crate) fn try_reclaim(mut self) -> Option<T> {
        self.item
            .take()
            .flatten()
            .and_then(|item| item.try_take())
    }
}

impl<'a, T> Future for RecvFut<'a, T> {
    type Output = Result<T, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        *this.item = if let Some(item) = this.item.take() {
            Some(match item {
                None => match match &this.receiver.0.flavor {
                    Flavor::Rendezvous(f) => wait_lock(f).recv(|| cx.waker().clone()).map(|(w, item)| { w.wake(); item }),
                    Flavor::Bounded(f) => wait_lock(f).recv(|| cx.waker().clone()).map(|(w, item)| { w.map(|w| w.wake()); item }),
                    Flavor::Unbounded(f) => wait_lock(f).recv(|| cx.waker().clone()),
                } {
                    Ok(item) => return Poll::Ready(Ok(item)),
                    Err(item) => Some(item),
                },
                Some(item) => if let Some(item) = item.try_take() {
                    return Poll::Ready(Ok(item));
                } else if this.receiver.0.is_disconnected() {
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

impl<'a, T> FusedFuture for RecvFut<'a, T> {
    fn is_terminated(&self) -> bool { self.item.is_none() }
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
