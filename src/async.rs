//! Futures and other types that allow asynchronous interaction with channels.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    fmt,
};
use crate::{*, signal::Signal};
use futures::{Stream, stream::FusedStream, future::FusedFuture, Sink};

struct AsyncSignal(Waker);

impl Signal for AsyncSignal {
    fn fire(&self) {
        self.0.wake_by_ref()
    }
}

// TODO: Wtf happens with timeout races? Futures can still receive items when not being actively polled...
// Is this okay? I guess it must be? How do other async channel crates handle it?

impl<T: Unpin> Sender<T> {
    /// Asynchronously send a value into the channel, returning an error if the channel receiver has
    /// been dropped. If the channel is bounded and is full, this method will yield to the async runtime.
    pub fn send_async(&self, item: T) -> impl Future<Output=Result<(), SendError<T>>> + '_ {
        SendFut {
            shared: &self.shared,
            slot: Some(Err(item)),
        }
    }

    /// Use this channel as an asynchronous item sink.
    pub fn sink(&self, item: T) -> impl Sink<T, Error=SendError<T>> + '_ {
        SendFut {
            shared: &self.shared,
            slot: None,
        }
    }
}

struct SendFut<'a, T: Unpin> {
    shared: &'a Shared<T>,
    // Only none after dropping
    slot: Option<Result<Arc<Slot<T, AsyncSignal>>, T>>,
}

impl<'a, T: Unpin> Drop for SendFut<'a, T> {
    fn drop(&mut self) {
        if let Some(Ok(slot)) = self.slot.take() {
            let slot: Arc<Slot<T, dyn Signal>> = slot;
            wait_lock(&self.shared.chan).sending.as_mut().unwrap().1.retain(|s| !Arc::ptr_eq(s, &slot));
        }
    }
}

impl<'a, T: Unpin> Future for SendFut<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(Ok(slot)) = self.slot.as_ref() {
            return if slot.is_empty() {
                Poll::Ready(Ok(()))
            } else if self.shared.is_disconnected() {
                match self.slot.take().unwrap() {
                    Err(item) => Poll::Ready(Err(SendError(item))),
                    Ok(slot) => match slot.try_take() {
                        Some(item) => Poll::Ready(Err(SendError(item))),
                        None => Poll::Ready(Ok(())),
                    },
                }
            } else {
                Poll::Pending
            };
        } else {
            self.shared.send(
                // item
                match self.slot.take().unwrap() {
                    Err(item) => item,
                    Ok(_) => return Poll::Ready(Ok(())),
                },
                // should_block
                true,
                // make_signal
                || AsyncSignal(cx.waker().clone()),
                // do_block
                |slot| {
                    self.slot = Some(Ok(slot));
                    Poll::Pending
                }
            )
                .map(|r| r.map_err(|err| match err {
                    TrySendTimeoutError::Disconnected(msg) => SendError(msg),
                    _ => unreachable!(),
                }))
        }
    }
}

impl<'a, T: Unpin> FusedFuture for SendFut<'a, T> {
    fn is_terminated(&self) -> bool {
        self.shared.is_disconnected()
    }
}

impl<'a, T: Unpin> Sink<T> for SendFut<'a, T> {
    type Error = SendError<T>;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.poll(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        *self = SendFut {
            shared: &self.shared,
            slot: Some(Err(item)),
        };

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.poll(cx) // TODO: A different strategy here?
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.poll(cx) // TODO: A different strategy here?
    }
}

impl<T> Receiver<T> {
    /// Asynchronously wait for an incoming value from the channel associated with this receiver,
    /// returning an error if all channel senders have been dropped.
    pub fn recv_async(&self) -> impl Future<Output=Result<T, RecvError>> + '_ {
        RecvFut::new(&self.shared)
    }

    /// Use this channel as an asynchronous stream of items.
    pub fn stream(&self) -> impl Stream<Item=T> + '_ {
        RecvFut::new(&self.shared)
    }
}

struct RecvFut<'a, T> {
    shared: &'a Shared<T>,
    slot: Option<Arc<Slot<T, AsyncSignal>>>,
}

impl<'a, T> RecvFut<'a, T> {
    fn new(shared: &'a Shared<T>) -> Self {
        Self {
            shared,
            slot: None,
        }
    }
}

impl<'a, T> Drop for RecvFut<'a, T> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            let slot: Arc<Slot<T, dyn Signal>> = slot;
            wait_lock(&self.shared.chan).waiting.retain(|s| !Arc::ptr_eq(s, &slot));
        }
    }
}

impl<'a, T> Future for RecvFut<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(slot) = self.slot.as_ref() {
            match slot.try_take() {
                Some(item) => Poll::Ready(Ok(item)),
                None => if self.shared.is_disconnected() {
                    Poll::Ready(Err(RecvError::Disconnected))
                } else {
                    Poll::Pending
                },
            }
        } else {
            self.shared.recv(
                // should_block
                true,
                // make_signal
                || AsyncSignal(cx.waker().clone()),
                // do_block
                |slot| {
                    self.slot = Some(slot);
                    Poll::Pending
                }
            )
                .map(|r| r.map_err(|err| match err {
                    TryRecvTimeoutError::Disconnected => RecvError::Disconnected,
                    _ => unreachable!(),
                }))
        }
    }
}

impl<'a, T> FusedFuture for RecvFut<'a, T> {
    fn is_terminated(&self) -> bool {
        self.shared.is_disconnected()
    }
}

impl<'a, T> Stream for RecvFut<'a, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(item) => {
                // Replace the recv future for every item we receive
                *self = RecvFut::new(self.shared);
                Poll::Ready(item.ok())
            },
        }
    }
}

impl<'a, T> FusedStream for RecvFut<'a, T> {
    fn is_terminated(&self) -> bool {
        self.shared.is_disconnected()
    }
}
