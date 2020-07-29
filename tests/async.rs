#[cfg(feature = "async")]
use flume::*;

#[cfg(feature = "async")]
#[test]
fn r#async_recv() {
    let (tx, mut rx) = unbounded();

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        tx.send(42u32).unwrap();
    });

    async_std::task::block_on(async {
        assert_eq!(rx.recv_async().await.unwrap(), 42);
    });

    t.join().unwrap();
}

#[cfg(feature = "async")]
#[test]
fn r#async_send() {
    let (tx, mut rx) = bounded(1);

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert_eq!(rx.recv(), Ok(42));
    });

    async_std::task::block_on(async {
        tx.send_async(42u32).await.unwrap();
    });

    t.join().unwrap();
}

#[cfg(feature = "async")]
#[test]
fn r#async_recv_disconnect() {
    let (tx, mut rx) = bounded::<i32>(0);

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(tx)
    });

    async_std::task::block_on(async {
        assert_eq!(rx.recv_async().await, Err(RecvError::Disconnected));
    });

    t.join().unwrap();
}

#[cfg(feature = "async")]
#[test]
fn r#async_send_disconnect() {
    let (tx, mut rx) = bounded(0);

    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(rx)
    });

    async_std::task::block_on(async {
        assert_eq!(tx.send_async(42u32).await, Err(SendError(42)));
    });

    t.join().unwrap();
}

#[cfg(feature = "async")]
#[async_std::test]
async fn send_100_million_no_drop_or_reorder() {
    #[derive(Debug)]
    enum Message {
        Increment {
            old: u64,
        },
        ReturnCount,
    }

    let (tx, mut rx) = unbounded();

    let t = async_std::task::spawn(async move {
        let mut count = 0u64;

        while let Ok(Message::Increment { old }) = rx.recv_async().await {
            assert_eq!(old, count);
            count += 1;
        }

        count
    });

    for next in 0..100_000_000 {
        tx.send(Message::Increment { old: next }).unwrap();
    }

    tx.send(Message::ReturnCount).unwrap();

    let count = t.await;
    assert_eq!(count, 100_000_000)
}
