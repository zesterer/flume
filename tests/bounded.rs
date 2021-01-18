#![cfg(feature = "sync")]

use flume::{bounded, TrySendError, TryRecvError};
use std::{
    time::{Instant, Duration},
    thread::{spawn, sleep},
};

fn ms(ms: u64) -> Duration { Duration::from_millis(ms) }

#[test]
fn basic() {
    let (tx, rx) = bounded(16);

    for i in 0..16 {
        tx.send(i).unwrap();
    }

    assert_eq!(tx.try_send(16), Err(TrySendError::Full(16)));

    spawn(move || {
        sleep(ms(250));
        tx.send(42).unwrap();
    });

    for i in 0..16 {
        assert_eq!(rx.recv(), Ok(i));
    }

    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

    let then = Instant::now();
    assert_eq!(Ok(42), rx.recv());
    let now = Instant::now();

    assert!(now.duration_since(then) > ms(250));
}
