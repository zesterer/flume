use std::time::{Instant, Duration};
use flume::*;

#[test]
fn send_recv() {
    let (tx, rx) = unbounded();
    for i in 0..1000 { tx.send(i).unwrap(); }
    for i in 0..1000 { assert_eq!(rx.try_recv().unwrap(), i); }
    assert!(rx.try_recv().is_err());
}

#[test]
fn iter() {
    let (tx, rx) = unbounded();
    for i in 0..1000 { tx.send(i).unwrap(); }
    drop(tx);
    assert_eq!(rx.iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn try_iter() {
    let (tx, rx) = unbounded();
    for i in 0..1000 { tx.send(i).unwrap(); }
    assert_eq!(rx.try_iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn iter_threaded() {
    let (tx, rx) = unbounded();
    for i in 0..1000 {
        let tx = tx.clone();
        std::thread::spawn(move || tx.send(i).unwrap());
    }
    drop(tx);
    assert_eq!(rx.iter().sum::<u32>(), (0..1000).sum());
}

#[test]
fn recv_timeout() {
    let (tx, rx) = unbounded();

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
    let (tx, rx) = unbounded();

    let dur = Duration::from_millis(350);
    let then = Instant::now();
    assert!(rx.recv_deadline(then.checked_add(dur).unwrap()).is_err());
    let now = Instant::now();

    let max_error = Duration::from_millis(10);
    assert!(now.duration_since(then) < dur.checked_add(max_error).unwrap());
    assert!(now.duration_since(then) > dur.checked_sub(max_error).unwrap());

    tx.send(42).unwrap();
    assert_eq!(rx.recv_deadline(now.checked_add(dur).unwrap()), Ok(42));
    assert!(Instant::now().duration_since(now) < max_error);
}

#[test]
fn disconnect_tx() {
    let (tx, rx) = unbounded::<()>();
    drop(tx);
    assert!(rx.recv().is_err());
}

#[test]
fn disconnect_rx() {
    let (tx, rx) = unbounded();
    drop(rx);
    assert!(tx.send(0).is_err());
}

#[test]
fn drain() {
    let (tx, rx) = unbounded();

    for i in 0..100 {
        tx.send(i).unwrap();
    }

    assert_eq!(rx.drain().sum::<u32>(), (0..100).sum());
}

#[test]
fn try_send() {
    let (tx, rx) = bounded(5);

    for i in 0..5 {
        tx.try_send(i).unwrap();
    }

    assert!(tx.try_send(42).is_err());

    assert_eq!(rx.recv(), Ok(0));

    assert_eq!(tx.try_send(42), Ok(()));

    assert_eq!(rx.recv(), Ok(1));
    drop(rx);

    assert!(tx.try_send(42).is_err());
}

#[test]
fn send_bounded() {
    let (tx, rx) = bounded(5);

    let mut ts = Vec::new();
    for _ in 0..100 {
        let tx = tx.clone();
        ts.push(std::thread::spawn(move || {
            for i in 0..10000 {
                tx.send(i).unwrap();
            }
        }));
    }

    drop(tx);

    assert_eq!(rx.iter().sum::<u64>(), (0..10000).sum::<u64>() * 100);

    for t in ts {
        t.join().unwrap();
    }

    assert!(rx.recv().is_err());
}

#[test]
fn rendezvous() {
    return; // TODO: Correct rendezvous behaviour

    let (tx, rx) = bounded(0);

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        rx.recv().unwrap();
    });

    tx.send(()).unwrap();

    t.join().unwrap();
}

#[test]
fn hydra() {
    let thread_num = 32;
    let msg_num = 1000;

    let (main_tx, main_rx) = unbounded::<()>();

    let mut txs = Vec::new();
    for _ in 0..thread_num {
        let main_tx = main_tx.clone();
        let (tx, rx) = unbounded();
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

#[test]
fn robin() {
    let thread_num = 32;
    let msg_num = 10;

    let (mut main_tx, main_rx) = bounded::<()>(1);

    for _ in 0..thread_num {
        let (mut tx, rx) = bounded(100);
        std::mem::swap(&mut tx, &mut main_tx);

        std::thread::spawn(move || {
            for msg in rx.iter() {
                tx.send(msg).unwrap();
            }
        });
    }

    for _ in 0..10000 {
        let main_tx = main_tx.clone();
        std::thread::spawn(move || {
            for _ in 0..msg_num {
                main_tx.send(Default::default()).unwrap();
            }
        });

        for _ in 0..msg_num {
            main_rx.recv().unwrap();
        }
    }
}

#[cfg(feature = "select")]
#[test]
fn select() {
    #[derive(Debug, PartialEq)]
    struct Foo(usize);

    let (tx0, rx0) = bounded(1);
    let (tx1, rx1) = unbounded();

    for (i, t) in vec![tx0.clone(), tx1].into_iter().enumerate() {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let _ = t.send(Foo(i));
        });
    }


    let x = Selector::new()
        .recv(&rx0, |x| x)
        .recv(&rx1, |x| x)
        .wait()
        .unwrap();

    if x == Foo(0) {
        assert!(rx1.recv().unwrap() == Foo(1));
    } else {
        assert!(rx0.recv().unwrap() == Foo(0));
    }

    tx0.send(Foo(42)).unwrap();

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(rx0.recv().unwrap(), Foo(42));
        assert_eq!(rx0.recv().unwrap(), Foo(43));

    });

    Selector::new()
        .send(&tx0, Foo(43), |x| x)
        .wait()
        .unwrap();

    t.join().unwrap();
}


#[cfg(feature = "async")]
#[test]
fn r#async() {
    let (tx, mut rx) = unbounded();

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        tx.send(42u32).unwrap();
    });

    async_std::task::block_on(async {
        assert_eq!(rx.recv_async().await.unwrap(), 42);
    });

    t.join().unwrap();
}

#[cfg(feature = "async")]
#[async_std::test]
async fn send_100_million_no_drop_or_reorder() {
    #[derive(Debug)]
    enum Message {
        Increment {
            old: u64,
        },
        ReturnCount,
    }

    let (tx, mut rx) = unbounded();

    let t = async_std::task::spawn(async move {
        let mut count = 0u64;

        while let Ok(Message::Increment { old }) = rx.recv_async().await {
            assert_eq!(old, count);
            count += 1;
        }

        count
    });

    for next in 0..100_000_000 {
        tx.send(Message::Increment { old: next }).unwrap();
    }

    tx.send(Message::ReturnCount).unwrap();

    let count = t.await;
    assert_eq!(count, 100_000_000)
}
