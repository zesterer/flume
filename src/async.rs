//! Futures and other types that allow asynchronous interaction with channels.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    any::Any,
    ops::Deref,
};
use crate::*;
use futures::{Stream, stream::FusedStream, future::FusedFuture, Sink};

struct AsyncSignal(Waker, AtomicBool);

impl Signal for AsyncSignal {
    fn fire(&self) {
        self.1.store(true, Ordering::SeqCst);
        self.0.wake_by_ref()
    }

    fn as_any(&self) -> &(dyn Any + 'static) { self }
}

#[derive(Clone)]
enum OwnedOrRef<'a, T> {
    Owned(T),
    Ref(&'a T),
}

impl<'a, T> Deref for OwnedOrRef<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self {
            OwnedOrRef::Owned(arc) => &arc,
            OwnedOrRef::Ref(r) => r,
        }
    }
}

impl<T: Unpin> Sender<T> {
    /// Asynchronously send a value into the channel, returning an error if the channel receiver has
    /// been dropped. If the channel is bounded and is full, this method will yield to the async runtime.
    pub fn send_async(&self, item: T) -> SendFuture<T> {
        SendFuture {
            sender: OwnedOrRef::Ref(&self),
            hook: Some(Err(item)),
        }
    }

    /// Use this channel as an asynchronous item sink. The returned stream holds a reference
    /// to the receiver.
    pub fn sink(&self) -> SendSink<'_, T> {
        SendSink(SendFuture {
            sender: OwnedOrRef::Ref(&self),
            hook: None,
        })
    }

    /// Use this channel as an asynchronous item sink. The returned stream has a `'static`
    /// lifetime.
    pub fn into_sink(self) -> SendSink<'static, T> {
        SendSink(SendFuture {
            sender: OwnedOrRef::Owned(self),
            hook: None,
        })
    }
}

/// A future that sends a value into a channel.
pub struct SendFuture<'a, T: Unpin> {
    sender: OwnedOrRef<'a, Sender<T>>,
    // Only none after dropping
    hook: Option<Result<Arc<Hook<T, AsyncSignal>>, T>>,
}

impl<'a, T: Unpin> Drop for SendFuture<'a, T> {
    fn drop(&mut self) {
        if let Some(Ok(hook)) = self.hook.take() {
            let hook: Arc<Hook<T, dyn Signal>> = hook;
            wait_lock(&self.sender.shared.chan).sending
                .as_mut()
                .unwrap().1
                .retain(|s| s.signal().as_any() as *const _ != hook.signal().as_any() as *const _);
        }
    }
}

impl<'a, T: Unpin> Future for SendFuture<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(Ok(hook)) = self.hook.as_ref() {
            return if hook.is_empty() {
                Poll::Ready(Ok(()))
            } else if self.sender.shared.is_disconnected() {
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
            let mut_self = self.get_mut();
            let shared = &mut_self.sender.shared;
            let this_hook = &mut mut_self.hook;

            shared.send(
                // item
                match this_hook.take().unwrap() {
                    Err(item) => item,
                    Ok(_) => return Poll::Ready(Ok(())),
                },
                // should_block
                true,
                // make_signal
                |msg| Hook::slot(Some(msg), AsyncSignal(cx.waker().clone(), AtomicBool::new(false))),
                // do_block
                |hook| {
                    *this_hook = Some(Ok(hook));
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
        self.sender.shared.is_disconnected()
    }
}

/// A sink that allows sending values into a channel.
pub struct SendSink<'a, T: Unpin>(SendFuture<'a, T>);

impl<'a, T: Unpin> Sink<T> for SendSink<'a, T> {
    type Error = SendError<T>;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        self.0 = SendFuture {
            sender: self.0.sender.clone(),
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
    pub fn recv_async(&self) -> RecvFut<'_, T> {
        RecvFut::new(OwnedOrRef::Ref(self))
    }

    /// Use this channel as an asynchronous stream of items. The returned stream holds a reference
    /// to the receiver.
    pub fn stream(&self) -> RecvStream<'_, T> {
        RecvStream(RecvFut::new(OwnedOrRef::Ref(self)))
    }

    /// Convert this channel into an asynchronous stream of items. The returned stream has a `'static`
    /// lifetime.
    pub fn into_stream(self) -> RecvStream<'static, T> {
        RecvStream(RecvFut::new(OwnedOrRef::Owned(self)))
    }
}

/// A future which allows asynchronously receiving a message.
pub struct RecvFut<'a, T> {
    receiver: OwnedOrRef<'a, Receiver<T>>,
    hook: Option<Arc<Hook<T, AsyncSignal>>>,
}

impl<'a, T> RecvFut<'a, T> {
    fn new(receiver: OwnedOrRef<'a, Receiver<T>>) -> Self {
        Self {
            receiver,
            hook: None,
        }
    }
}

impl<'a, T> Drop for RecvFut<'a, T> {
    fn drop(&mut self) {
        if let Some(hook) = self.hook.take() {
            let hook: Arc<Hook<T, dyn Signal>> = hook;
            let mut chan = wait_lock(&self.receiver.shared.chan);
            // We'd like to use `Arc::ptr_eq` here but it doesn't seem to work consistently with wide pointers?
            chan.waiting.retain(|s| s.signal().as_any() as *const _ != hook.signal().as_any() as *const _);
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

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.hook.is_some() {
            if let Ok(msg) = self.receiver.shared.recv_sync(None) {
                Poll::Ready(Ok(msg))
            } else if self.receiver.shared.is_disconnected() {
                Poll::Ready(Err(RecvError::Disconnected))
            } else {
                Poll::Pending
            }
        } else {
            let mut_self = self.get_mut();
            let shared = &(mut_self.receiver.shared);
            let this_hook = &mut (mut_self.hook);

            shared.recv(
                // should_block
                true,
                // make_signal
                || Hook::trigger(AsyncSignal(cx.waker().clone(), AtomicBool::new(false))),
                // do_block
                |hook| {
                    *this_hook = Some(hook);
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
        self.receiver.shared.is_disconnected()
    }
}

/// A stream which allows asynchronously receiving messages.
pub struct RecvStream<'a, T>(RecvFut<'a, T>);

impl<'a, T> Stream for RecvStream<'a, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(item) => {
                // Replace the recv future for every item we receive
                self.0 = RecvFut::new(self.0.receiver.clone());
                Poll::Ready(item.ok())
            },
        }
    }
}

impl<'a, T> FusedStream for RecvStream<'a, T> {
    fn is_terminated(&self) -> bool {
        self.0.is_terminated()
    }
}
