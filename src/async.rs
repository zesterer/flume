//! Futures and other types that allow asynchronous interaction with channels.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    any::Any,
};
use crate::*;
use futures::{Stream, stream::FusedStream, future::FusedFuture, Sink};

struct AsyncSignal(Waker, AtomicBool);

impl Signal for AsyncSignal {
    fn as_any(&self) -> &(dyn Any + 'static) { self }

    fn fire(&self) {
        self.1.store(true, Ordering::SeqCst);
        self.0.wake_by_ref()
    }
}

// TODO: Wtf happens with timeout races? Futures can still receive items when not being actively polled...
// Is this okay? I guess it must be? How do other async channel crates handle it?

impl<T: Unpin> Sender<T> {
    /// Asynchronously send a value into the channel, returning an error if the channel receiver has
    /// been dropped. If the channel is bounded and is full, this method will yield to the async runtime.
    pub fn send_async(&self, item: T) -> SendFuture<T> {
        SendFuture {
            shared: &self.shared,
            hook: Some(Err(item)),
        }
    }

    /// Use this channel as an asynchronous item sink.
    pub fn sink(&self) -> SendSink<T> {
        SendSink(SendFuture {
            shared: &self.shared,
            hook: None,
        })
    }
}

pub struct SendFuture<'a, T: Unpin> {
    shared: &'a Shared<T>,
    // Only none after dropping
    hook: Option<Result<Arc<Hook<T, AsyncSignal>>, T>>,
}

impl<'a, T: Unpin> Drop for SendFuture<'a, T> {
    fn drop(&mut self) {
        if let Some(Ok(hook)) = self.hook.take() {
            let hook: Arc<Hook<T, dyn Signal>> = hook;
            wait_lock(&self.shared.chan).sending.as_mut().unwrap().1.retain(|s| !Arc::ptr_eq(s, &hook));
        }
    }
}

impl<'a, T: Unpin> Future for SendFuture<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(Ok(hook)) = self.hook.as_ref() {
            return if hook.is_empty() {
                Poll::Ready(Ok(()))
            } else if self.shared.is_disconnected() {
                match self.hook.take().unwrap() {
                    Err(item) => Poll::Ready(Err(SendError(item))),
                    Ok(hook) => match hook.try_take() {
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
                match self.hook.take().unwrap() {
                    Err(item) => item,
                    Ok(_) => return Poll::Ready(Ok(())),
                },
                // should_block
                true,
                // make_signal
                |msg| Hook::slot(Some(msg), AsyncSignal(cx.waker().clone(), AtomicBool::new(false))),
                // do_block
                |hook| {
                    self.hook = Some(Ok(hook));
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

impl<'a, T: Unpin> FusedFuture for SendFuture<'a, T> {
    fn is_terminated(&self) -> bool {
        self.shared.is_disconnected()
    }
}

pub struct SendSink<'a, T: Unpin>(SendFuture<'a, T>);

impl<'a, T: Unpin> Sink<T> for SendSink<'a, T> {
    type Error = SendError<T>;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        self.0 = SendFuture {
            shared: &self.0.shared,
            hook: Some(Err(item)),
        };

        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx) // TODO: A different strategy here?
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx) // TODO: A different strategy here?
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
    hook: Option<Arc<Hook<T, AsyncSignal>>>,
}

impl<'a, T> RecvFut<'a, T> {
    fn new(shared: &'a Shared<T>) -> Self {
        Self {
            shared,
            hook: None,
        }
    }
}

impl<'a, T> Drop for RecvFut<'a, T> {
    fn drop(&mut self) {
        if let Some(hook) = self.hook.take() {
            let hook: Arc<Hook<T, dyn Signal>> = hook;
            let mut chan = wait_lock(&self.shared.chan);
            chan.waiting.retain(|s| Arc::as_ptr(s) as *const () != Arc::as_ptr(&hook) as *const ());
            if hook.signal().as_any().downcast_ref::<AsyncSignal>().unwrap().1.load(Ordering::SeqCst) {
                // If this signal has been fired, but we're being dropped (and so not listening to it),
                // pass the signal on to another receiver
                chan.try_wake_receiver_if_pending();
            }
        }
    }
}

impl<'a, T> Future for RecvFut<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.hook.is_some() {
            if let Ok(msg) = self.shared.recv_sync(None) {
                Poll::Ready(Ok(msg))
            } else if self.shared.is_disconnected() {
                Poll::Ready(Err(RecvError::Disconnected))
            } else {
                Poll::Pending
            }
        } else {
            self.shared.recv(
                // should_block
                true,
                // make_signal
                || Hook::trigger(AsyncSignal(cx.waker().clone(), AtomicBool::new(false))),
                // do_block
                |hook| {
                    self.hook = Some(hook);
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
