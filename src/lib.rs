#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]

extern crate alloc;

pub mod sender;
pub mod receiver;
#[cfg(feature = "select")] pub mod select;

mod chan;
mod rendezvous;
mod bounded;
mod unbounded;

pub use crate::{
    chan::{SendFut, RecvFut},
    sender::Sender,
    receiver::Receiver,
};

#[cfg(feature = "select")] pub use select::Selector;

use core::{
    task::{Waker, Poll, Context},
    pin::Pin,
    future::Future,
    sync::atomic::{AtomicUsize, AtomicBool, Ordering},
};
use alloc::{sync::Arc, collections::VecDeque, borrow::Cow};
use pin_project_lite::pin_project;
#[cfg(feature = "std")] use std::time::{Duration, Instant};

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
