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
#[test]
fn r#async_recv_drop_recv() {
    let (tx, mut rx) = bounded::<i32>(10);

    let recv_fut = rx.recv_async();

    async_std::task::block_on(async {
        let _ = async_std::future::timeout(std::time::Duration::from_millis(500), rx.recv_async()).await;
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
async fn send_1_million_no_drop_or_reorder() {
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
#[test]
fn async_no_double_wake() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::pin::Pin;
    use std::task::Context;
    use futures::task::{waker, ArcWake};
    use futures::Stream;

    let mut count = Arc::new(AtomicUsize::new(0));

    // all this waker does is count how many times it is called
    struct CounterWaker {
        count: Arc<AtomicUsize>,
    }
    
    impl ArcWake for CounterWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    // create waker and context
    let w = CounterWaker {
        count: count.clone(),
    };
    let w = waker(Arc::new(w));
    let cx = &mut Context::from_waker(&w);
    
    // create unbounded channel
    let (tx, mut rx) = unbounded::<()>();
    let mut stream = rx.stream();

    // register waker with stream
    Pin::new(&mut stream).poll_next(cx);

    // send multiple items
    tx.send(());
    tx.send(());
    tx.send(());

    // verify that stream is only woken up once.
    assert_eq!(count.load(Ordering::SeqCst), 1);
}