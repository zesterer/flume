use std::time::{Instant, Duration};
use flume::*;

#[test]
fn send_recv() {
    let (tx, rx) = channel();
    for i in 0..1000 { tx.send(i).unwrap(); }
    for i in 0..1000 { assert_eq!(rx.try_recv().unwrap(), i); }
    assert!(rx.try_recv().is_err());
}

#[test]
fn iter() {
    let (tx, rx) = channel();
    for i in 0..1000 { tx.send(i).unwrap(); }
    drop(tx);
    assert_eq!(rx.iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn try_iter() {
    let (tx, rx) = channel();
    for i in 0..1000 { tx.send(i).unwrap(); }
    assert_eq!(rx.try_iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn iter_threaded() {
    let (tx, rx) = channel();
    for i in 0..1000 {
        let tx = tx.clone();
        std::thread::spawn(move || tx.send(i).unwrap());
    }
    drop(tx);
    assert_eq!(rx.iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn recv_timeout() {
    let (tx, rx) = channel();

    let dur = Duration::from_millis(350);
    let then = Instant::now();
    assert!(rx.recv_timeout(dur).is_err());
    let now = Instant::now();

    let max_error = Duration::from_millis(1);
    assert!(now.duration_since(then) < dur.checked_add(max_error).unwrap());
    assert!(now.duration_since(then) > dur.checked_sub(max_error).unwrap());

    tx.send(42).unwrap();
    assert_eq!(rx.recv_timeout(dur), Ok(42));
    assert!(Instant::now().duration_since(now) < max_error);
}

#[test]
fn recv_deadline() {
    let (tx, rx) = channel();

    let dur = Duration::from_millis(350);
    let then = Instant::now();
    assert!(rx.recv_deadline(then.checked_add(dur).unwrap()).is_err());
    let now = Instant::now();

    let max_error = Duration::from_millis(1);
    assert!(now.duration_since(then) < dur.checked_add(max_error).unwrap());
    assert!(now.duration_since(then) > dur.checked_sub(max_error).unwrap());

    tx.send(42).unwrap();
    assert_eq!(rx.recv_deadline(now.checked_add(dur).unwrap()), Ok(42));
    assert!(Instant::now().duration_since(now) < max_error);
}

#[test]
fn disconnect_tx() {
    let (tx, rx) = channel::<()>();
    drop(tx);
    assert!(rx.recv().is_err());
}

#[test]
fn disconnect_rx() {
    let (tx, rx) = channel();
    drop(rx);
    assert!(tx.send(0).is_err());
}

#[test]
fn hydra() {
    let thread_num = 32;
    let msg_num = 1000;

    let (main_tx, main_rx) = channel::<()>();

    let mut txs = Vec::new();
    for _ in 0..thread_num {
        let main_tx = main_tx.clone();
        let (tx, rx) = channel();
        txs.push(tx);

        std::thread::spawn(move || {
            for msg in rx.iter() {
                main_tx.send(msg).unwrap();
            }
        });
    }

    drop(main_tx);

    for _ in 0..10000 {
        for tx in &txs {
            for _ in 0..msg_num {
                tx.send(Default::default()).unwrap();
            }
        }

        for _ in 0..thread_num {
            for _ in 0..msg_num {
                main_rx.recv().unwrap();
            }
        }
    }

    drop(txs);
    assert!(main_rx.recv().is_err());
}
