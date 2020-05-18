use std::time::{Instant, Duration};
use flume::*;

#[cfg(not(feature = "loom"))]
mod loom {
    pub use std::thread;
    pub use std::sync;

    pub fn model<F>(f: F)
    where
        F: Fn() + Sync + Send + 'static
    {
        f()
    }
}

use loom::model;


#[test]
fn send_recv() {
    model(|| {
        let (tx, rx) = unbounded();
        for i in 0..10 { tx.send(i).unwrap(); }
        for i in 0..10 { assert_eq!(rx.try_recv().unwrap(), i); }
        assert!(rx.try_recv().is_err());
    });
}

#[test]
fn iter() {
    model(|| {
        let (tx, rx) = unbounded();
        for i in 0..10 { tx.send(i).unwrap(); }
        drop(tx);
        assert_eq!(rx.iter().sum::<u32>(), (0..10).sum());
    });
}

#[test]
fn try_iter() {
    model(|| {
        let (tx, rx) = unbounded();
        for i in 0..10 { tx.send(i).unwrap(); }
        assert_eq!(rx.try_iter().sum::<u32>(), (0..10).sum());
    });
}

#[test]
fn iter_threaded() {
    model(|| {
        let (tx, rx) = unbounded();
        for i in 0..3 {
            let tx = tx.clone();
            loom::thread::spawn(move || tx.send(i).unwrap());
        }
        drop(tx);
        assert_eq!(rx.iter().sum::<u32>(), (0..3).sum());
    });
}

#[test]
fn recv_timeout() {
    model(|| {
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
    });
}

#[test]
fn recv_deadline() {
    model(|| {
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
    });
}

#[test]
fn disconnect_tx() {
    model(|| {
        let (tx, rx) = unbounded::<()>();
        drop(tx);
        assert!(rx.recv().is_err());
    });
}

#[test]
fn disconnect_rx() {
    model(|| {
        let (tx, rx) = unbounded();
        drop(rx);
        assert!(tx.send(0).is_err());
    });
}

#[test]
fn drain() {
    model(|| {
        let (tx, rx) = unbounded();

        for i in 0..10 {
            tx.send(i).unwrap();
        }

        assert_eq!(rx.drain().sum::<u32>(), (0..10).sum());

        for i in 0..10 {
            tx.send(i).unwrap();
        }

        for i in 0..10 {
            tx.send(i).unwrap();
        }

        rx.recv().unwrap();

        (1u32..10).chain(0..10).zip(rx).for_each(|(l, r)| assert_eq!(l, r));
    });
}

#[test]
fn try_send() {
    model(|| {
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
    });
}

#[test]
fn send_bounded() {
    model(|| {
        let (tx, rx) = bounded(5);

        for _ in 0..5 {
            tx.send(42).unwrap();
        }

        let _ = rx.recv().unwrap();

        tx.send(42).unwrap();

        assert!(tx.try_send(42).is_err());

        rx.drain();

        let mut ts = Vec::new();
        for _ in 0..3 {
            let tx = tx.clone();
            ts.push(loom::thread::spawn(move || {
                for i in 0..10 {
                    tx.send(i).unwrap();
                }
            }));
        }

        drop(tx);

        assert_eq!(rx.iter().sum::<u64>(), (0..10).sum::<u64>() * 3);

        for t in ts {
            t.join().unwrap();
        }

        assert!(rx.recv().is_err());
    });
}

#[test]
fn rendezvous() {
    model(|| {
        let (tx, rx) = bounded(0);

        for i in 0..3 {
            let tx = tx.clone();
            let t = loom::thread::spawn(move || {
                assert!(tx.try_send(()).is_err());

                let then = Instant::now();
                tx.send(()).unwrap();
                let now = Instant::now();

                assert!(now.duration_since(then) > Duration::from_millis(50), "iter = {}", i);
            });

            std::thread::sleep(Duration::from_millis(250));
            rx.recv().unwrap();

            t.join().unwrap();
        }
    });
}

#[test]
fn hydra() {
    model(|| {
        let thread_num = 3;
        let msg_num = 10;

        let (main_tx, main_rx) = unbounded::<()>();

        let mut txs = Vec::new();
        for _ in 0..thread_num {
            let main_tx = main_tx.clone();
            let (tx, rx) = unbounded();
            txs.push(tx);

            loom::thread::spawn(move || {
                for msg in rx.iter() {
                    main_tx.send(msg).unwrap();
                }
            });
        }

        drop(main_tx);

        for _ in 0..3 {
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
    });
}

#[test]
fn robin() {
    model(|| {
        let thread_num = 2;
        let msg_num = 10;

        let (mut main_tx, main_rx) = bounded::<()>(1);

        for _ in 0..thread_num {
            let (mut tx, rx) = bounded(100);
            std::mem::swap(&mut tx, &mut main_tx);

            loom::thread::spawn(move || {
                for msg in rx.iter() {
                    tx.send(msg).unwrap();
                }
            });
        }

        for _ in 0..1 {
            let main_tx = main_tx.clone();
            loom::thread::spawn(move || {
                for _ in 0..msg_num {
                    main_tx.send(Default::default()).unwrap();
                }
            });

            for _ in 0..msg_num {
                main_rx.recv().unwrap();
            }
        }
    });
}

#[cfg(feature = "select")]
#[test]
fn select() {
    model(|| {
        #[derive(Debug, PartialEq)]
        struct Foo(usize);

        let (tx0, rx0) = bounded(1);
        let (tx1, rx1) = unbounded();

        for (i, t) in vec![tx0.clone(), tx1].into_iter().enumerate() {
            loom::thread::spawn(move || {
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

        let t = loom::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert_eq!(rx0.recv().unwrap(), Foo(42));
            assert_eq!(rx0.recv().unwrap(), Foo(43));

        });

        Selector::new()
            .send(&tx0, Foo(43), |x| x)
            .wait()
            .unwrap();

        t.join().unwrap();
    });
}
