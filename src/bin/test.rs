#![feature(test)]

extern crate test;

use std::thread;
use flume;

fn main() {
    const THREAD_NUM: usize = 32;
    const MSG_NUM: usize = 1;

    for _ in 0..1000 {
        let (tx, mut rx) = flume::channel::<i32>();

        for i in 0..THREAD_NUM {
            let mut main_tx = tx.clone();
            let (mut tx, mut rx) = flume::channel();

            for _ in 0..MSG_NUM {
                tx.send(Default::default());
            }

            thread::spawn(move || {
                for msg in rx.iter() {
                    main_tx.send(msg);
                }
                //println!("FINISH! {} / {}", i + 1, THREAD_NUM);
            });
        }

        drop(tx);

        let mut total = 0;
        for (i, msg) in rx.iter().enumerate() {
            test::black_box(msg);
            total += 1;
            //println!("RECV: {} / {}", i + 1, THREAD_NUM);
        }

        assert_eq!(total, THREAD_NUM * MSG_NUM);
    }
}
