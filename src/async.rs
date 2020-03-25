//! Futures and other types that allow asynchronous interaction with channels.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use crate::*;
use futures::{Stream, stream::FusedStream, future::FusedFuture, Sink};

impl<T> Receiver<T> {
    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let mut buf = self.buffer.borrow_mut();

        let res = if let Some(msg) = buf.pop_front() {
            return Poll::Ready(Ok(msg)); // Avoid the need to lock the inner
        } else {
            self
                .shared
                .poll_inner()
                .map(|mut inner| self
                    .shared
                    .try_recv(
                        move || {
                            // Detach the waker
                            inner.recv_waker = None;
                            // Inform the sender that we no longer need waking
                            inner.listen_mode = 1;
                            inner
                        },
                        &mut buf,
                        &self.finished,
                    )
                )
        };

        let poll = match res {
            Some(Ok(msg)) => Poll::Ready(Ok(msg)),
            Some(Err((_, TryRecvError::Disconnected))) => Poll::Ready(Err(RecvError::Disconnected)),
            Some(Err((mut inner, TryRecvError::Empty))) => {
                // Inform the sender that we need waking
                inner.recv_waker = Some(cx.waker().clone());
                inner.listen_mode = 2;
                Poll::Pending
            },
            // Can't access the inner lock, try again
            None => {
                cx.waker().wake_by_ref();
                Poll::Pending
            },
        };

        poll
    }
}

/// A future  used to receive a value from the channel.
pub struct RecvFuture<'a, T> {
    recv: &'a mut Receiver<T>,
}

impl<'a, T> RecvFuture<'a, T> {
    pub(crate) fn new(recv: &mut Receiver<T>) -> RecvFuture<T> {
        RecvFuture { recv }
    }
}

impl<'a, T> Future for RecvFuture<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.recv.poll(cx)
    }
}

impl<'a, T> FusedFuture for RecvFuture<'a, T> {
    fn is_terminated(&self) -> bool {
        self.recv.finished.get()
    }
}

enum PollSendError<T> {
    Full(T),
    Disconnected(T),
    CouldNotLock(T),
}

impl<T> PollSendError<T> {
    fn into_message(self) -> T {
        match self {
            PollSendError::Full(m) => m,
            PollSendError::Disconnected(m) => m,
            PollSendError::CouldNotLock(m) => m,
        }
    }
}

impl<T> Sender<T> {
    pub fn sink(&self) -> SenderSink<T> {
        SenderSink::new(self)
    }

    #[inline]
    fn poll(&self, msg: T, cx: &mut Context<'_>) -> Result<(), PollSendError<T>> {
        let mut inner = match self.shared.poll_inner() {
            Some(inner) => inner,
            None => {
                cx.waker().wake_by_ref();
                return Err(PollSendError::CouldNotLock(msg));
            },
        };

        if inner.listen_mode == 0 {
            // If the listener has disconnected, the channel is dead
            return Err(PollSendError::Disconnected(msg));
        }
        // If pushing fails, it's because the queue is full
        match inner.queue.push(msg) {
            Err(msg) => {
                // Let the receiver know that we need to be woken
                inner.send_wakers.push_back(cx.waker().clone());
                return Err(PollSendError::Full(msg))
            },
            Ok(()) => {},
        };

        self.shared.send_notify(inner);
        Ok(())
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll(cx).map(|ready| ready.ok())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.buffer.borrow().len(), None)
    }
}

impl<T> FusedStream for Receiver<T> {
    fn is_terminated(&self) -> bool {
        self.finished.get()
    }
}

/// Error returned when a send future or sink linked to a disconnected sender is polled. To retrieve
/// the associated message, call the `into_message` method of the `SendFuture` or `into_pending`
/// method of the `SinkSender`.
pub struct Disconnected;

pub struct SendFuture<'a, T> {
    send: &'a Sender<T>,
    msg: Option<T>,
}

impl<'a, T> SendFuture<'a, T> {
    pub(crate) fn new(send: &Sender<T>, msg: T) -> SendFuture<T> {
        SendFuture { send, msg: Some(msg) }
    }
}

impl<'a, T> SendFuture<'a, T> {
    pub fn into_message(self) -> Option<T> {
        self.msg
    }
}

impl<'a, T: Unpin> Future for SendFuture<'a, T> {
    type Output = Result<(), Disconnected>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.msg.take() {
            Some(msg) => match self.send.poll(msg, cx) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(PollSendError::Disconnected(m)) => {
                    self.msg.replace( m);
                    Poll::Ready(Err(Disconnected))
                },
                Err(e) => { // Need to wait a bit... poll will be rescheduled by `send.poll`
                    self.msg.replace(e.into_message());
                    Poll::Pending
                },
            },
            None => Poll::Pending,
        }
    }
}

impl<'a, T: Unpin> FusedFuture for SendFuture<'a, T> {
    fn is_terminated(&self) -> bool {
        self.send.disconnected.get()
    }
}

pub struct SenderSink<'a, T> {
    buf: VecDeque<T>,
    send: &'a Sender<T>,
}

impl<'a, T> SenderSink<'a, T> {
    fn new(send: &'a Sender<T>) -> Self {
        SenderSink {
            buf: VecDeque::new(),
            send,
        }
    }

    pub fn into_pending(self) -> Vec<T> {
        self.buf.into()
    }
}

impl<'a, T: Unpin> Sink<T> for SenderSink<'a, T> {
    type Error = Disconnected;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if !self.send.disconnected.get() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(Disconnected))
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        if !self.send.disconnected.get() {
            self.buf.push_back(item);
            Ok(())
        } else {
            Err(Disconnected)
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.send.disconnected.get() {
            return Poll::Ready(Err(Disconnected));
        }

        match self.buf.pop_front() {
            Some(msg) => match self.send.poll(msg, cx) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(PollSendError::Disconnected(m)) => {
                    self.buf.push_front(m);
                    Poll::Ready(Err(Disconnected))
                },
                Err(e) => { // Need to wait a bit... poll will be rescheduled by `send.poll`
                    self.buf.push_front(e.into_message());
                    Poll::Pending
                },
            },
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}
