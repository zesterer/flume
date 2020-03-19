//! Futures and other types that allow asynchronous interaction with channels.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use crate::*;

/// A future  used to receive a value from the channel.
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
        // Set the waker. Cannot be done in the constructor as context is not provided yet
        self.recv.shared.with_inner(|mut inner| inner.recv_waker = Some(cx.waker().clone()));

        // On success, set the waker to none to avoid it being woken again in case that is wrong
        // TODO: `poll_recv` instead to prevent even spinning?
        let res = self.recv.shared.try_recv(|mut inner| inner.recv_waker = None);

        let poll = match res {
            Ok(msg) => Poll::Ready(Ok(msg)),
            Err((_guard, TryRecvError::Disconnected)) => Poll::Ready(Err(RecvError::Disconnected)),
            Err((_guard, TryRecvError::Empty)) => Poll::Pending,
            // TODO: Uhhh should we have this?
            /*Err(None) => {
                //cx.waker().wake_by_ref();
                Poll::Pending
            }*/
        };

        if poll.is_ready() && self.set_as_waiting {
            // Inform the sender that we no longer need waking
            self.recv.shared.with_inner(|mut inner| inner.listen_mode -= 1);
        } else if poll.is_pending() && !self.set_as_waiting {
            // Inform the sender that we need waking
            self.recv.shared.with_inner(|mut inner| inner.listen_mode += 1);
        }

        poll
    }
}
