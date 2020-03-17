#[cfg(feature = "select")]
use flume::Selector;

#[cfg(feature = "select")]
fn main() {
    let (red_tx, red_rx) = flume::unbounded();
    let (blue_tx, blue_rx) = flume::unbounded();

    std::thread::spawn(move || { let _ = red_tx.send("Red"); });
    std::thread::spawn(move || { let _ = blue_tx.send("Blue"); });

    let winner = Selector::new()
        .recv(&red_rx, |msg| msg)
        .recv(&blue_rx, |msg| msg)
        .wait()
        .unwrap();

    println!("{} won!", winner);
}

#[cfg(not(feature = "select"))]
fn main() {}
