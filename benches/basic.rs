#[macro_use]
extern crate criterion;

use std::{
    sync::mpsc,
    thread,
};
use criterion::{Criterion, Bencher, black_box};

trait Sender: Clone + Send + Sized + 'static {
    type Item: Default;
    type Receiver: Receiver<Item=Self::Item>;

    fn channel() -> (Self, Self::Receiver);
    fn send(&mut self, msg: Self::Item);
}

trait Receiver: Send + Sized + 'static {
    type Item: Default;
    fn recv(&mut self) -> Self::Item;
    fn iter(&mut self) -> Box<dyn Iterator<Item=Self::Item> + '_>;
}

impl<T: Send + Default + 'static> Sender for flume::Sender<T> {
    type Item = T;
    type Receiver = flume::Receiver<T>;

    fn channel() -> (Self, Self::Receiver) {
        flume::channel()
    }

    fn send(&mut self, msg: T) {
        flume::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for flume::Receiver<T> {
    type Item = T;

    fn recv(&mut self) -> Self::Item {
        flume::Receiver::recv(self).unwrap()
    }

    fn iter(&mut self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(flume::Receiver::iter(self))
    }
}

impl<T: Send + Default + 'static> Sender for crossbeam_channel::Sender<T> {
    type Item = T;
    type Receiver = crossbeam_channel::Receiver<T>;

    fn channel() -> (Self, Self::Receiver) {
        crossbeam_channel::unbounded()
    }

    fn send(&mut self, msg: T) {
        crossbeam_channel::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for crossbeam_channel::Receiver<T> {
    type Item = T;

    fn recv(&mut self) -> Self::Item {
        crossbeam_channel::Receiver::recv(self).unwrap()
    }

    fn iter(&mut self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(crossbeam_channel::Receiver::iter(self))
    }
}

impl<T: Send + Default + 'static> Sender for mpsc::Sender<T> {
    type Item = T;
    type Receiver = mpsc::Receiver<T>;

    fn channel() -> (Self, Self::Receiver) {
        mpsc::channel()
    }

    fn send(&mut self, msg: T) {
        mpsc::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for mpsc::Receiver<T> {
    type Item = T;

    fn recv(&mut self) -> Self::Item {
        mpsc::Receiver::recv(self).unwrap()
    }

    fn iter(&mut self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(mpsc::Receiver::iter(self))
    }
}






fn test_create<S: Sender>(b: &mut Bencher) {
    b.iter(|| S::channel());
}

fn test_oneshot<S: Sender>(b: &mut Bencher) {
    b.iter(|| {
        let (mut tx, mut rx) = S::channel();
        tx.send(Default::default());
        black_box(rx.recv());
    });
}

fn test_inout<S: Sender>(b: &mut Bencher) {
    let (mut tx, mut rx) = S::channel();
    b.iter(|| {
        tx.send(Default::default());
        black_box(rx.recv());
    });
}

fn test_hydra<S: Sender>(b: &mut Bencher, thread_num: usize, msg_num: usize) {
    b.iter(|| {
        let (tx, mut rx) = S::channel();

        for _ in 0..thread_num {
            let mut main_tx = tx.clone();
            let (mut tx, mut rx) = S::channel();

            for _ in 0..msg_num {
                tx.send(Default::default());
            }

            thread::spawn(move || {
                for msg in rx.iter() {
                    main_tx.send(msg);
                }
            });
        }

        drop(tx);

        let mut total = 0;
        for msg in rx.iter() {
            black_box(msg);
            total += 1;
        }

        assert_eq!(total, thread_num * msg_num);
    });
}



fn create(b: &mut Criterion) {
    b.bench_function("create-flume", |b| test_create::<flume::Sender<u32>>(b));
    b.bench_function("create-crossbeam", |b| test_create::<crossbeam_channel::Sender<u32>>(b));
    b.bench_function("create-std", |b| test_create::<mpsc::Sender<u32>>(b));
}

fn oneshot(b: &mut Criterion) {
    b.bench_function("oneshot-flume", |b| test_oneshot::<flume::Sender<u32>>(b));
    b.bench_function("oneshot-crossbeam", |b| test_oneshot::<crossbeam_channel::Sender<u32>>(b));
    b.bench_function("oneshot-std", |b| test_oneshot::<mpsc::Sender<u32>>(b));
}

fn inout(b: &mut Criterion) {
    b.bench_function("inout-flume", |b| test_inout::<flume::Sender<u32>>(b));
    b.bench_function("inout-crossbeam", |b| test_inout::<crossbeam_channel::Sender<u32>>(b));
    b.bench_function("inout-std", |b| test_inout::<mpsc::Sender<u32>>(b));
}

fn hydra_32t_1m(b: &mut Criterion) {
    b.bench_function("hydra-32t-1m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 32, 1));
    b.bench_function("hydra-32t-1m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 32, 1));
    b.bench_function("hydra-32t-1m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 32, 1));
}

fn hydra_32t_1000m(b: &mut Criterion) {
    b.bench_function("hydra-32t-1000m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 32, 1000));
    b.bench_function("hydra-32t-1000m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 32, 1000));
    b.bench_function("hydra-32t-1000m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 32, 1000));
}

fn hydra_1t_1000m(b: &mut Criterion) {
    b.bench_function("hydra-1t-1000m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 1, 1000));
    b.bench_function("hydra-1t-1000m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 1, 1000));
    b.bench_function("hydra-1t-1000m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 1, 1000));
}

criterion_group!(compare, create, oneshot, inout, hydra_32t_1m, hydra_32t_1000m, hydra_1t_1000m);
criterion_main!(compare);
