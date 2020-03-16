use std::thread;
use flume::Selector;

fn main() {
    let (red_tx, red_rx) = flume::unbounded();
    let (blue_tx, blue_rx) = flume::unbounded();

    let mut get_winner = Selector::new()
        .with(&red_rx, |msg| msg)
        .with(&blue_rx, |msg| msg);

    thread::spawn(move || { let _ = red_tx.send("Red"); });
    thread::spawn(move || { let _ = blue_tx.send("Blue"); });

    println!("{} won!", get_winner.recv());
}
