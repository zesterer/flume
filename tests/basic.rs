#[test]
fn send_recv() {
    let (tx, rx) = flume::channel();

    for i in 0..1000 {
        tx.send(i).unwrap();
    }

    for i in 0..1000 {
        assert_eq!(rx.try_recv().unwrap(), i);
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn iter() {
    let (tx, rx) = flume::channel();

    for i in 0..1000 {
        tx.send(i).unwrap();
    }

    for (i, msg) in rx.try_iter().enumerate() {
        assert_eq!(msg, i);
    }
}

#[test]
fn disconnect() {
    let (tx, rx) = flume::channel();

    drop(rx);

    assert!(tx.send(0).is_err());
}
