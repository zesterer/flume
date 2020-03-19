use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use crate::*;

pub struct RecvFuture<'a, T> {
    set_as_waiting: bool,
    recv: &'a mut Receiver<T>,
}

impl<'a, T> RecvFuture<'a, T> {
    pub(crate) fn new(recv: &mut Receiver<T>) -> RecvFuture<T> {
        RecvFuture {
            set_as_waiting: false,
            recv,
        }
    }
}

impl<'a, T> Future for RecvFuture<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let recv = &self.recv;

        // Set the waker. Cannot be done in the constructor as context is not provided yet
        *recv.shared.recv_waker.lock() = Some(cx.waker().clone());

        // On success, set the waker to none to avoid it being woken again in case that is wrong
        let res = recv.shared.poll_recv(|shared| *shared.recv_waker.lock() = None);

        let poll = match res {
            Ok(msg) => Poll::Ready(Ok(msg)),
            Err(Some((_guard, TryRecvError::Disconnected))) => Poll::Ready(Err(RecvError::Disconnected)),
            Err(Some((_guard, TryRecvError::Empty))) => Poll::Pending,
            Err(None) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        };

        if poll.is_ready() && self.set_as_waiting {
            // Inform the sender that we no longer need waking
            self.recv.shared.listen_mode.fetch_sub(1, Ordering::Release);
        } else if poll.is_pending() && !self.set_as_waiting {
            // Inform the sender that we need waking
            self.recv.shared.listen_mode.fetch_add(1, Ordering::Acquire);
        }

        poll
    }
}
