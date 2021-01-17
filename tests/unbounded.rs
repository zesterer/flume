#![cfg(feature = "sync")]

use flume2::unbounded;

#[test]
fn basic() {
    let (tx, rx) = unbounded();

    tx.send(42).unwrap();

    assert_eq!(rx.recv(), Ok(42));
}
