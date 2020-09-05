use std::{thread::{self, Thread}, time::Duration, any::Any};

pub trait Signal: Send + Sync + 'static {
    /// Fire the signal, returning whether it is a stream signal
    // TODO(stream) explain why this matters
    fn fire(&self) -> bool;
    fn as_any(&self) -> &(dyn Any + 'static);
    fn as_ptr(&self) -> *const ();
}

pub struct SyncSignal(Thread);

impl Default for SyncSignal {
    fn default() -> Self {
        Self(thread::current())
    }
}

impl Signal for SyncSignal {
    fn fire(&self) -> bool {
        self.0.unpark();
        false
    }
    fn as_any(&self) -> &(dyn Any + 'static) { self }
    fn as_ptr(&self) -> *const () { self as *const _ as *const () }
}

impl SyncSignal {
    pub fn wait(&self) { thread::park(); }
    pub fn wait_timeout(&self, dur: Duration) { thread::park_timeout(dur); }
}
