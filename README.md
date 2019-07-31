# Flume

A blazingly fast multi-producer channel.

```rs
let (tx, rx) = flume::channel();

thread::spawn(move || {
    for i in 0..10 {
        tx.send(i);
    }
});

let received = rx
    .iter()
    .sum();

assert_eq!((0..10).sum(), received);
```

## Performance

Flume is considerably faster than `std`'s `mpsc`, and on par with `crossbeam_channel` for most metrics.

*TODO: More in-depth numbers*

## License

Flume is licensed under either of:

- Apache License 2.0, (http://www.apache.org/licenses/LICENSE-2.0)

- MIT license (http://opensource.org/licenses/MIT)
