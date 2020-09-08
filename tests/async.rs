#[cfg(feature = "async")]
use {
    flume::*,
    futures::{stream::FuturesUnordered, StreamExt, TryFutureExt},
    async_std::prelude::FutureExt,
    std::time::Duration
};

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
#[test]
fn r#async_recv_drop_recv() {
    let (tx, mut rx) = bounded::<i32>(10);

    let recv_fut = rx.recv_async();

    async_std::task::block_on(async {
        let res = async_std::future::timeout(std::time::Duration::from_millis(500), rx.recv_async()).await;
        assert!(res.is_err());
    });

    let rx2 = rx.clone();
    let t = std::thread::spawn(move || {
        async_std::task::block_on(async {
            rx2.recv_async().await
        })
    });

    std::thread::sleep(std::time::Duration::from_millis(500));

    tx.send(42).unwrap();

    drop(recv_fut);

    assert_eq!(t.join().unwrap(), Ok(42))
}

#[cfg(feature = "async")]
#[async_std::test]
async fn r#async_send_1_million_no_drop_or_reorder() {
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

    for next in 0..1_000_000 {
        tx.send(Message::Increment { old: next }).unwrap();
    }

    tx.send(Message::ReturnCount).unwrap();

    let count = t.await;
    assert_eq!(count, 1_000_000)
}

#[cfg(feature = "async")]
#[async_std::test]
async fn parallel_async_receivers() {
    let (tx, rx) = flume::unbounded();
    let send_fut = async move {
        let n_sends: usize = 100000;
        for _ in 0..n_sends {
            tx.send_async(()).await.unwrap();
        }
    };

    async_std::task::spawn(
        send_fut
            .timeout(Duration::from_secs(5))
            .map_err(|_| panic!("Send timed out!"))
    );

    let mut futures_unordered = (0..250)
        .map(|_| async {
            while let Ok(()) = rx.recv_async().await
            /* rx.recv() is OK */
            {}
        })
        .collect::<FuturesUnordered<_>>();

    let recv_fut = async {
        while futures_unordered.next().await.is_some() {}
    };

    recv_fut
        .timeout(Duration::from_secs(5))
        .map_err(|_| panic!("Receive timed out!"))
        .await;

    println!("recv end");
}
