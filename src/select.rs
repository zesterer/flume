//! Types that permit waiting upon multiple blocking operations using the [`Selector`] interface.

use crate::*;
use std::{
    any::Any,
    marker::PhantomData,
};

// A unique token corresponding to an event in a selector
type Token = usize;

struct SelectSignal(thread::Thread, Token, AtomicBool, Arc<Spinlock<VecDeque<Token>>>);

impl Signal for SelectSignal {
    fn fire(&self) {
        self.2.store(true, Ordering::SeqCst);
        self.3.lock().push_back(self.1);
        self.0.unpark();
    }

    fn as_any(&self) -> &(dyn Any + 'static) { self }
}

trait Selection<'a, T> {
    fn init(&mut self) -> Option<T>;
    fn poll(&mut self) -> Option<T>;
    fn deinit(&mut self);
}

/// A type used to wait upon multiple blocking operations at once.
///
/// A [`Selector`] implements [`select`](https://en.wikipedia.org/wiki/Select_(Unix))-like behaviour,
/// allowing a thread to wait upon the result of more than one operation at once.
///
/// # Examples
/// ```
/// let (tx0, rx0) = flume::unbounded();
/// let (tx1, rx1) = flume::unbounded();
///
/// std::thread::spawn(move || {
///     tx0.send(true).unwrap();
///     tx1.send(42).unwrap();
/// });
///
/// flume::Selector::new()
///     .recv(&rx0, |b| println!("Received {:?}", b))
///     .recv(&rx1, |n| println!("Received {:?}", n))
///     .wait();
/// ```
pub struct Selector<'a, T: 'a> {
    selections: Vec<Box<dyn Selection<'a, T> + 'a>>,
    next_poll: usize,
    signalled: Arc<Spinlock<VecDeque<Token>>>,
}

impl<'a, T> Selector<'a, T> {
    /// Create a new selector.
    pub fn new() -> Self {
        Self {
            selections: Vec::new(),
            next_poll: 0,
            signalled: Arc::default(),
        }
    }

    /// Add a send operation to the selector that sends the provided value.
    ///
    /// Once added, the selector can be used to run the provided handler function on completion of
    /// this operation.
    pub fn send<U>(mut self, sender: &'a Sender<U>, msg: U, mapper: impl FnMut(Result<(), SendError<U>>) -> T + 'a) -> Self {
        struct SendSelection<'a, T, F, U> {
            sender: &'a Sender<U>,
            msg: Option<U>,
            token: Token,
            signalled: Arc<Spinlock<VecDeque<Token>>>,
            hook: Option<Arc<Hook<U, SelectSignal>>>,
            mapper: F,
            phantom: PhantomData<T>,
        }

        impl<'a, T, F, U> Selection<'a, T> for SendSelection<'a, T, F, U>
            where F: FnMut(Result<(), SendError<U>>) -> T
        {
            fn init(&mut self) -> Option<T> {
                let token = self.token;
                let signalled = self.signalled.clone();
                let r = self.sender.shared.send(
                    self.msg.take().unwrap(),
                    true,
                    |msg| Hook::slot(Some(msg), SelectSignal(thread::current(), token, AtomicBool::new(false), signalled)),
                    // Always runs
                    |h| { self.hook = Some(h); Ok(()) },
                );

                if self.hook.is_none() {
                    Some((self.mapper)(match r {
                        Ok(()) => Ok(()),
                        Err(TrySendTimeoutError::Disconnected(msg)) => Err(SendError(msg)),
                        _ => unreachable!(),
                    }))
                } else {
                    None
                }
            }

            fn poll(&mut self) -> Option<T> {
                let res = if self.sender.shared.is_disconnected() {
                    // Check the hook one last time
                    if let Some(msg) = self.hook.as_ref()?.try_take() {
                        Err(SendError(msg))
                    } else {
                        Ok(())
                    }
                } else if self.hook.as_ref().unwrap().is_empty() {
                    // The message was sent
                    Ok(())
                } else {
                    return None
                };

                Some((&mut self.mapper)(res))
            }

            fn deinit(&mut self) {
                if let Some(hook) = self.hook.take() {
                    // Remove hook
                    let hook: Arc<Hook<U, dyn Signal>> = hook;
                    wait_lock(&self.sender.shared.chan).sending
                        .as_mut()
                        .unwrap().1
                        .retain(|s| !Arc::ptr_eq(s, &hook));
                }
            }
        }

        let token = self.selections.len();
        self.selections.push(Box::new(SendSelection {
            sender,
            msg: Some(msg),
            token,
            signalled: self.signalled.clone(),
            hook: None,
            mapper,
            phantom: Default::default(),
        }));

        self
    }

    /// Add a receive operation to the selector.
    ///
    /// Once added, the selector can be used to run the provided handler function on completion of
    /// this operation.
    pub fn recv<U>(mut self, receiver: &'a Receiver<U>, mapper: impl FnMut(Result<U, RecvError>) -> T + 'a) -> Self {
        struct RecvSelection<'a, T, F, U> {
            receiver: &'a Receiver<U>,
            token: Token,
            signalled: Arc<Spinlock<VecDeque<Token>>>,
            hook: Option<Arc<Hook<U, SelectSignal>>>,
            mapper: F,
            received: bool,
            phantom: PhantomData<T>,
        }

        impl<'a, T, F, U> Selection<'a, T> for RecvSelection<'a, T, F, U>
            where F: FnMut(Result<U, RecvError>) -> T
        {
            fn init(&mut self) -> Option<T> {
                let token = self.token;
                let signalled = self.signalled.clone();
                let r = self.receiver.shared.recv(
                    true,
                    || Hook::trigger(SelectSignal(thread::current(), token, AtomicBool::new(false), signalled)),
                    // Always runs
                    |h| { self.hook = Some(h); Err(TryRecvTimeoutError::Timeout) },
                );

                if self.hook.is_none() {
                    Some((self.mapper)(match r {
                        Ok(msg) => Ok(msg),
                        Err(TryRecvTimeoutError::Disconnected) => Err(RecvError::Disconnected),
                        _ => unreachable!(),
                    }))
                } else {
                    None
                }
            }

            fn poll(&mut self) -> Option<T> {
                let res = if let Ok(msg) = self.receiver.try_recv() {
                    self.received = true;
                    Ok(msg)
                } else if self.receiver.shared.is_disconnected() {
                    Err(RecvError::Disconnected)
                } else {
                    return None;
                };

                Some((&mut self.mapper)(res))
            }

            fn deinit(&mut self) {
                if let Some(hook) = self.hook.take() {
                    // Remove hook
                    let hook: Arc<Hook<U, dyn Signal>> = hook;
                    wait_lock(&self.receiver.shared.chan).waiting.retain(|s| !Arc::ptr_eq(s, &hook));
                    // If we were woken, but never polled, wake up another
                    if !self.received && hook.signal().as_any().downcast_ref::<SelectSignal>().unwrap().2.load(Ordering::SeqCst) {
                        wait_lock(&self.receiver.shared.chan).try_wake_receiver_if_pending();
                    }
                }
            }
        }

        let token = self.selections.len();
        self.selections.push(Box::new(RecvSelection {
            receiver,
            token,
            signalled: self.signalled.clone(),
            hook: None,
            mapper,
            received: false,
            phantom: Default::default(),
        }));

        self
    }

    fn wait_inner(&mut self, deadline: Option<Instant>) -> Option<T> {
        let res = 'outer: loop {
            // Init signals
            for s in &mut self.selections {
                if let Some(msg) = s.init() {
                    break 'outer Some(msg);
                }
            }

            // Speculatively poll
            if let Some(msg) = self.poll() {
                break 'outer Some(msg);
            }

            loop {
                if let Some(deadline) = deadline {
                    if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                        thread::park_timeout(dur);
                    }
                } else {
                    thread::park();
                }

                if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                    break 'outer self.poll();
                }

                let token = if let Some(token) = self.signalled.lock().pop_front() {
                    token
                } else {
                    // Spurious wakeup, park again
                    continue;
                };

                // Attempt to receive a message
                if let Some(msg) = self.selections[token].poll() {
                    break 'outer Some(msg);
                }
            }
        };

        // Deinit signals
        for s in &mut self.selections {
            s.deinit();
        }

        res
    }

    /// Poll each event associated with this [`Selector`] to see whether any have completed. If
    /// more than one event has completed, a random event handler will be run and its return value
    /// returned. If none of the events have completed a `None` is returned.
    pub fn poll(&mut self) -> Option<T> {
        for _ in 0..self.selections.len() {
            if let Some(val) = self.selections[self.next_poll].poll() {
                return Some(val);
            }
            self.next_poll = (self.next_poll + 1) % self.selections.len();
        }
        None
    }

    /// Wait until one of the events associated with this [`Selector`] has completed. If more than
    /// one event has completed, a random event handler will be run and its return value produced.
    pub fn wait(mut self) -> T {
        self.wait_inner(None).unwrap()
    }

    /// Wait until one of the events associated with this [`Selector`] has completed or the timeout
    /// has been reached. If more than one event has completed, a random event handler will be run
    /// and its return value produced.
    pub fn wait_timeout(mut self, dur: Duration) -> T {
        self.wait_inner(Some(Instant::now() + dur)).unwrap()
    }

    // /// Create an iterator over incoming events on this [`Selector`].
    // pub fn iter(&mut self) -> SelectorIter<'a, '_, T> {
    //     SelectorIter { selector: self }
    // }

    // /// Create an iterator over pending events on this [`Selector`]. This iterator will only
    // /// produce values while there are events immediately available to handle.
    // pub fn try_iter(&mut self) -> SelectorTryIter<'a, '_, T> {
    //     SelectorTryIter { selector: self }
    // }
}

// /// An iterator over the events received by a [`Selector`].
// pub struct SelectorIter<'a, 'b, T> {
//     selector: &'b mut Selector<'a, T>,
// }

// impl<'a, 'b, T> Iterator for SelectorIter<'a, 'b, T> {
//     type Item = T;

//     fn next(&mut self) -> Option<Self::Item> {
//         Some(self.selector.wait_inner())
//     }
// }

// /// An iterator over the pending events received by a [`Selector`].
// pub struct SelectorTryIter<'a, 'b, T> {
//     selector: &'b mut Selector<'a, T>,
// }

// impl<'a, 'b, T> Iterator for SelectorTryIter<'a, 'b, T> {
//     type Item = T;

//     fn next(&mut self) -> Option<Self::Item> {
//         self.selector.poll()
//     }
// }
