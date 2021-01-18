#![cfg(feature = "sync")]

use flume::{rendezvous, SendError};
use std::{
    time::{Instant, Duration},
    thread::{spawn, sleep},
};

fn ms(ms: u64) -> Duration { Duration::from_millis(ms) }

#[test]
fn basic() {
    let (tx, rx) = rendezvous();

    spawn(move || {
        sleep(ms(250));
        tx.send(42).unwrap();
    });

    let then = Instant::now();
    assert_eq!(Ok(42), rx.recv());
    let now = Instant::now();

    assert!(now.duration_since(then) > ms(250));
}

#[test]
fn drop_recv() {
    let (tx, rx) = rendezvous();

    spawn(move || {
        sleep(ms(250));
        drop(rx);
    });

    let then = Instant::now();
    assert_eq!(Err(SendError::Disconnected(42)), tx.send(42));
    let now = Instant::now();

    assert!(now.duration_since(then) > ms(250));
}
