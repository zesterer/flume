#[cfg(feature = "select")]
fn main() {
    use flume::{Selector, unbounded};
    use rand::prelude::*;

    // Create two channels
    let (red_tx, red_rx) = unbounded();
    let (blue_tx, blue_rx) = unbounded();

    // To make it fair, randomise the start order
    let mut racers = vec![("Red", red_tx), ("Blue", blue_tx)];
    racers.shuffle(&mut thread_rng());

    for (color, tx) in racers {
        std::thread::spawn(move || { let _ = tx.send(color); });
    }

    const RED: usize = 0;
    const BLUE: usize = 1;

    let mut sel = Selector::new();
    let red = sel.recv(RED, &red_rx);
    let blue = sel.recv(BLUE, &blue_rx);

    // Race them to see which one sends their message first
    let winner = match sel.wait() {
        RED => red.get().unwrap(),
        BLUE => blue.get().unwrap(),
        _ => unreachable!(),
    };

    println!("{} won!", winner);
}

#[cfg(not(feature = "select"))]
fn main() {}
