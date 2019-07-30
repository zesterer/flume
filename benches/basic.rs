#![feature(test)]

extern crate test;

use std::sync::mpsc;
use test::Bencher;

#[bench]
fn flume(b: &mut Bencher) {
    b.iter(|| {
        let (tx, rx) = flume::channel();

        for i in 0..1000 {
            tx.send(i);
        }

        for (i, msg) in rx.try_iter().enumerate() {
            assert_eq!(msg, i);
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

        for (i, msg) in rx.try_iter().enumerate() {
            assert_eq!(msg, i);
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

        for (i, msg) in rx.try_iter().enumerate() {
            assert_eq!(msg, i);
        }
        assert!(rx.try_recv().is_err());
    });
}
