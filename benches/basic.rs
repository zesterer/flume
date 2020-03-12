#[macro_use]
extern crate criterion;

use std::{
    sync::mpsc,
    thread,
    fmt::Debug,
};
use criterion::{Criterion, Bencher, black_box};

trait Sender: Clone + Send + Sized + 'static {
    type Item: Debug + Default;
    type Receiver: Receiver<Item=Self::Item>;

    fn channel() -> (Self, Self::Receiver);
    fn send(&self, msg: Self::Item);
}

trait Receiver: Send + Sized + 'static {
    type Item: Default;
    fn recv(&self) -> Self::Item;
    fn iter(&self) -> Box<dyn Iterator<Item=Self::Item> + '_>;
}

impl<T: Send + Debug + Default + 'static> Sender for flume::Sender<T> {
    type Item = T;
    type Receiver = flume::Receiver<T>;

    fn channel() -> (Self, Self::Receiver) {
        flume::channel()
    }

    fn send(&self, msg: T) {
        flume::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for flume::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        flume::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(flume::Receiver::iter(self))
    }
}

impl<T: Send + Debug + Default + 'static> Sender for crossbeam_channel::Sender<T> {
    type Item = T;
    type Receiver = crossbeam_channel::Receiver<T>;

    fn channel() -> (Self, Self::Receiver) {
        crossbeam_channel::unbounded()
    }

    fn send(&self, msg: T) {
        crossbeam_channel::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for crossbeam_channel::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        crossbeam_channel::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(crossbeam_channel::Receiver::iter(self))
    }
}

impl<T: Send + Debug + Default + 'static> Sender for mpsc::Sender<T> {
    type Item = T;
    type Receiver = mpsc::Receiver<T>;

    fn channel() -> (Self, Self::Receiver) {
        mpsc::channel()
    }

    fn send(&self, msg: T) {
        mpsc::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for mpsc::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        mpsc::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(mpsc::Receiver::iter(self))
    }
}

fn test_create<S: Sender>(b: &mut Bencher) {
    b.iter(|| S::channel());
}

fn test_oneshot<S: Sender>(b: &mut Bencher) {
    b.iter(|| {
        let (tx, rx) = S::channel();
        tx.send(Default::default());
        black_box(rx.recv());
    });
}

fn test_inout<S: Sender>(b: &mut Bencher) {
    let (tx, rx) = S::channel();
    b.iter(|| {
        tx.send(Default::default());
        black_box(rx.recv());
    });
}

fn test_hydra<S: Sender>(b: &mut Bencher, thread_num: usize, msg_num: usize) {
    let (main_tx, main_rx) = S::channel();

    let mut txs = Vec::new();
    for _ in 0..thread_num {
        let main_tx = main_tx.clone();
        let (tx, rx) = S::channel();
        txs.push(tx);

        thread::spawn(move || {
            for msg in rx.iter() {
                main_tx.send(msg);
            }
        });
    }

    drop(main_tx);

    b.iter(|| {
        for tx in &txs {
            for _ in 0..msg_num {
                tx.send(Default::default());
            }
        }

        for _ in 0..thread_num {
            for _ in 0..msg_num {
                black_box(main_rx.recv());
            }
        }
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

fn hydra_4t_10000m(b: &mut Criterion) {
    b.bench_function("hydra-4t-10000m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 4, 10000));
    b.bench_function("hydra-4t-10000m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 4, 10000));
    b.bench_function("hydra-4t-10000m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 4, 10000));
}

criterion_group!(compare, create, oneshot, inout, hydra_32t_1m, hydra_32t_1000m, hydra_1t_1000m, hydra_4t_10000m);
criterion_main!(compare);
