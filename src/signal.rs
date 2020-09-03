use std::{thread::{self, Thread}, time::Duration, any::Any};

pub trait Signal: Send + Sync + 'static {
    fn fire(&self);
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
    fn fire(&self) { self.0.unpark(); }
    fn as_any(&self) -> &(dyn Any + 'static) { self }
    fn as_ptr(&self) -> *const () { self as *const _ as *const () }
}

impl SyncSignal {
    pub fn wait(&self) { thread::park(); }
    pub fn wait_timeout(&self, dur: Duration) { thread::park_timeout(dur); }
}
