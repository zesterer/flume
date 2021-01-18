use super::*;

pub(crate) enum Flavor<T> {
    Rendezvous(Rendezvous<T>),
    Bounded(Bounded<T>),
    Unbounded(Unbounded<T>),
}

pub(crate) struct Channel<T> {
    flavor: Lock<Flavor<T>>,
    disconnected: AtomicBool,
    pub sends: AtomicUsize,
    pub recvs: AtomicUsize,
}

impl<T> Channel<T> {
    pub fn new(flavor: Flavor<T>) -> Self {
        Self {
            flavor: Lock::new(flavor),
            disconnected: AtomicBool::new(false),
            sends: AtomicUsize::new(1),
            recvs: AtomicUsize::new(1),
        }
    }

    pub fn disconnect(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
        match &mut *wait_lock(&self.flavor) {
            Flavor::Rendezvous(f) => f.disconnect(),
            Flavor::Bounded(f) => f.disconnect(),
            Flavor::Unbounded(f) => f.disconnect(),
        }
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Relaxed)
    }

    pub fn len_cap(&self) -> (usize, Option<usize>) {
        match &mut *wait_lock(&self.flavor) {
            Flavor::Rendezvous(f) => f.len_cap(),
            Flavor::Bounded(f) => f.len_cap(),
            Flavor::Unbounded(f) => f.len_cap(),
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
        match &mut *wait_lock(&self.flavor) {
            Flavor::Rendezvous(f) => f.drain(),
            Flavor::Bounded(f) => f.drain(),
            Flavor::Unbounded(f) => f.drain(),
        }
    }

    pub fn try_send(&self, item: T) -> Result<(), (T, bool)> {
        match &mut *wait_lock(&self.flavor) {
            Flavor::Rendezvous(f) => f.try_send(item).map(|w| w.wake()),
            Flavor::Bounded(f) => f.try_send(item).map(|w| { w.map(|w| w.wake()); }),
            Flavor::Unbounded(f) => f.try_send(item).map(|w| { w.map(|w| w.wake()); }),
        }
    }

    pub fn try_recv(&self) -> Result<T, bool> {
        match &mut *wait_lock(&self.flavor) {
            Flavor::Rendezvous(f) => f.try_recv().map(|(waker, item)| { waker.wake(); item }),
            Flavor::Bounded(f) => f.try_recv().map(|(waker, item)| { waker.map(|w| w.wake()); item }),
            Flavor::Unbounded(f) => f.try_recv(),
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
                Ok(item) => item.try_take_deactivate(),
                Err(item) => Some(item),
            })
    }
}

impl<'a, T> Future for SendFut<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        *this.item = if let Some(item) = this.item.take() {
            Some(match item {
                Err(item) => match match &mut *wait_lock(&this.sender.0.flavor) {
                    Flavor::Rendezvous(f) => f.send(item, || cx.waker().clone()).map(|w| w.wake()),
                    Flavor::Bounded(f) => f.send(item, || cx.waker().clone()).map(|w| { w.map(|w| w.wake()); }),
                    Flavor::Unbounded(f) => Ok(f.send(item).map(|w| { w.map(|w| w.wake()); }).unwrap_or(())),
                } {
                    Ok(()) => return Poll::Ready(Ok(())),
                    Err(Err(item)) => return Poll::Ready(Err(SendError::Disconnected(item))),
                    Err(Ok(item)) => Ok(item),
                },
                Ok(item) => if !item.is_full() {
                    return Poll::Ready(Ok(()));
                } else if this.sender.0.is_disconnected() {
                    match item.try_take_deactivate() {
                        Some(item) => return Poll::Ready(Err(SendError::Disconnected(item))),
                        None => return Poll::Ready(Ok(())),
                    }
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
            .and_then(|item| item.try_take_deactivate())
    }
}

impl<'a, T> Future for RecvFut<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        *this.item = if let Some(item) = this.item.take() {
            Some(match item {
                None => match match &mut *wait_lock(&this.receiver.0.flavor) {
                    Flavor::Rendezvous(f) => f.recv(|| cx.waker().clone()).map(|(w, item)| { w.wake(); item }),
                    Flavor::Bounded(f) => f.recv(|| cx.waker().clone()).map(|(w, item)| { w.map(|w| w.wake()); item }),
                    Flavor::Unbounded(f) => f.recv(|| cx.waker().clone()),
                } {
                    Ok(item) => return Poll::Ready(Ok(item)),
                    Err(None) => return Poll::Ready(Err(RecvError::Disconnected)),
                    Err(Some(item)) => Some(item),
                },
                Some(item) => if let Some(item) = item.try_take() {
                    return Poll::Ready(Ok(item));
                } else if this.receiver.0.is_disconnected() {
                    if let Some(item) = item.try_take() {
                        return Poll::Ready(Ok(item));
                    } else {
                        return Poll::Ready(Err(RecvError::Disconnected));
                    }
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
    pub(crate) inner: Arc<spin::mutex::SpinMutex<(Option<T>, bool)>>,
}

impl<T> Clone for Booth<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<T> Booth<T> {
    pub fn full(item: T) -> Self {
        Self { inner: Arc::new(spin::mutex::SpinMutex::new((Some(item), true))), }
    }

    pub fn empty() -> Self {
        Self { inner: Arc::new(spin::mutex::SpinMutex::new((None, true))), }
    }

    pub fn give(&self, item: T) -> Option<T> {
        let mut guard = self.inner.lock();
        if guard.1 {
            debug_assert!(guard.0.is_none());
            guard.0 = Some(item);
            None
        } else {
            Some(item)
        }
    }

    pub fn try_take(&self) -> Option<T> {
        let mut guard = self.inner.lock();
        guard.0.take().filter(|_| guard.1)
    }

    pub fn try_take_deactivate(&self) -> Option<T> {
        let mut guard = self.inner.lock();
        debug_assert_eq!(guard.1, true);
        guard.1 = false;
        guard.0.take().map(|item| item)
    }

    pub fn is_full(&self) -> bool {
        self.inner.lock().0.is_some()
    }
}
