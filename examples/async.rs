#[cfg(feature = "async")]
#[async_std::main]
async fn main() {
    let (tx, rx) = flume::unbounded();

    let t = async_std::task::spawn(async move {
        for msg in rx.iter() {
            println!("Received: {}", msg);
        }
    });

    tx.send("Hello, world!").unwrap();
    tx.send("How are you today?").unwrap();

    drop(tx);

    t.await;
}

#[cfg(not(feature = "async"))]
fn main () {}
