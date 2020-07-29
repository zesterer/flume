use std::{thread::{self, Thread}, time::Duration};

pub trait Signal: Send + Sync + 'static {
    fn fire(&self);
}

pub struct SyncSignal(Thread);

impl Default for SyncSignal {
    fn default() -> Self {
        Self(thread::current())
    }
}

impl Signal for SyncSignal {
    fn fire(&self) { self.0.unpark(); }
}

impl SyncSignal {
    pub fn wait(&self) { thread::park(); }
    pub fn wait_timeout(&self, dur: Duration) { thread::park_timeout(dur); }
}
