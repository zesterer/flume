use std::thread;
use flume::Selector;

fn main() {
    let (red_tx, red_rx) = flume::unbounded();
    let (blue_tx, blue_rx) = flume::unbounded();

    let get_winner = Selector::new()
        .recv(&red_rx, |msg| msg)
        .recv(&blue_rx, |msg| msg);

    thread::spawn(move || { let _ = red_tx.send("Red"); });
    thread::spawn(move || { let _ = blue_tx.send("Blue"); });

    println!("{} won!", get_winner.wait().unwrap());
}
