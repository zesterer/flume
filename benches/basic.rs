#![feature(test)]

extern crate test;

use std::sync::mpsc;
use test::{Bencher, black_box};

#[bench]
fn flume(b: &mut Bencher) {
    b.iter(|| {
        let (tx, rx) = flume::channel();

        for i in 0..1000 {
            tx.send(i);
        }

        for msg in rx.try_iter() {
            black_box(msg);
        }
        assert!(rx.try_recv().is_none());
    });
}

#[bench]
fn crossbeam(b: &mut Bencher) {
    b.iter(|| {
        let (tx, rx) = crossbeam_channel::unbounded();

        for i in 0..1000 {
            tx.send(i).unwrap();
        }

        for msg in rx.try_iter() {
            black_box(msg);
        }
        assert!(rx.try_recv().is_err());
    });
}

#[bench]
fn std(b: &mut Bencher) {
    b.iter(|| {
        let (tx, rx) = mpsc::channel();

        for i in 0..1000 {
            tx.send(i).unwrap();
        }

        for msg in rx.try_iter() {
            black_box(msg);
        }
        assert!(rx.try_recv().is_err());
    });
}
