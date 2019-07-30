#![feature(test)]

use flume;
use std::{
    thread,
    hint::black_box,
};

fn main() {
    const THREAD_NUM: usize = 32;
    const MSG_NUM: usize = 1000;

    for _ in 0..1000 {
        let (tx, rx) = flume::channel::<i32>();

        for _ in 0..THREAD_NUM {
            let main_tx = tx.clone();
            let (tx, rx) = flume::channel();

            for _ in 0..MSG_NUM {
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

        assert_eq!(total, THREAD_NUM * MSG_NUM);
    }
}
