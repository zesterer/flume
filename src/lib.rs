#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

mod sender;
mod receiver;
#[cfg(feature = "sink")]
#[cfg_attr(docsrs, doc(cfg(feature = "sink")))]
mod sink;
#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
mod stream;
#[cfg(feature = "select")]
#[cfg_attr(docsrs, doc(cfg(feature = "select")))]
pub mod select;

mod chan;
mod rendezvous;
mod bounded;
mod unbounded;

pub use crate::{
    chan::{SendFut, RecvFut},
    sender::{Sender, IntoSendFut},
    receiver::{Receiver, IntoRecvFut, TryIter, IntoTryIter, Drain, IntoDrain},
};

#[cfg(feature = "sync")] pub use crate::receiver::{Iter, IntoIter};

#[cfg(feature = "sink")]
#[cfg_attr(docsrs, doc(cfg(feature = "sink")))]
pub use crate::sink::SendSink;
#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
pub use crate::stream::RecvStream;
#[cfg(feature = "select")]
#[cfg_attr(docsrs, doc(cfg(feature = "select")))]
pub use crate::select::Selector;

use core::{
    task::{Waker, Poll, Context},
    pin::Pin,
    future::Future,
    marker::PhantomData,
    sync::atomic::{AtomicUsize, AtomicBool, Ordering},
};
#[cfg(feature = "std")] use std::time::Duration;
#[cfg(feature = "time")] use std::time::Instant;
use alloc::{sync::Arc, collections::VecDeque, borrow::Cow};
use pin_project_lite::pin_project;
use futures_core::future::FusedFuture;

use crate::{
    chan::{Channel, Flavor, Booth},
    rendezvous::Rendezvous,
    bounded::Bounded,
    unbounded::Unbounded,
};

#[cfg(all(windows, feature = "std"))] type Lock<T> = std::sync::Mutex<T>;
#[cfg(all(windows, feature = "std"))] type LockGuard<'a, T> = std::sync::MutexGuard<'a, T>;

#[cfg(not(all(windows, feature = "std")))] type Lock<T> = spin::mutex::SpinMutex<T>;
#[cfg(not(all(windows, feature = "std")))] type LockGuard<'a, T> = spin::mutex::SpinMutexGuard<'a, T>;

#[cfg(all(not(windows), feature = "std"))]
fn wait_lock<'a, T>(lock: &'a Lock<T>) -> LockGuard<'a, T> {
    use std::thread;
    let mut i = 4;
    loop {
        for _ in 0..10 {
            if let Some(guard) = lock.try_lock() {
                return guard;
            }
            thread::yield_now();
        }
        thread::sleep(Duration::from_nanos(1 << i));
        i += 1;
    }
}

#[cfg(all(windows, feature = "std"))]
fn wait_lock<'a, T>(lock: &'a Lock<T>) -> LockGuard<'a, T> {
    lock.lock().unwrap()
}

#[cfg(any(windows, not(feature = "std")))]
fn wait_lock<'a, T>(lock: &'a Lock<T>) -> LockGuard<'a, T> {
    lock.lock()
}

pub fn rendezvous<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Channel::new(Rendezvous::new()));
    (Sender(chan.clone()), Receiver(chan))
}

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    assert!(cap > 0, "Bounded channels with a size of 0 are rendezvous channels. Use `flume::rendezvous` instead.");
    let chan = Arc::new(Channel::new(Bounded::new(cap)));
    (Sender(chan.clone()), Receiver(chan))
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Channel::new(Unbounded::new()));
    (Sender(chan.clone()), Receiver(chan))
}
