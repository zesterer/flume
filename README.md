# Flume

A blazingly fast multi-producer channel.

```rust
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

Flume is considerably faster than `std`'s `mpsc`, and on par with `crossbeam_channel` for most benchmarks.

```
Create a channel
----------------
[#2] create-flume            time:   [119.10 ns]
[#3] create-crossbeam        time:   [217.45 ns]
[#1] create-std              time:   [51.040 ns]

Create a channel, then send/recv a single message on it
-------------------------------------------------------
[#2] oneshot-flume           time:   [132.17 ns]
[#3] oneshot-crossbeam       time:   [291.33 ns]
[#1] oneshot-std             time:   [70.489 ns]

Send/recv single messages on an existing channel
------------------------------------------------
[#1] inout-flume             time:   [16.542 ns]
[#3] inout-crossbeam         time:   [50.070 ns]
[#2] inout-std               time:   [36.533 ns]

Send 1 message to 32 threads, then send 1 message back from each
------------------------------------------------------------------
[#1] hydra-32t-1m-flume      time:   [377.14 us]
[#3] hydra-32t-1m-crossbeam  time:   [389.42 us]
[#2] hydra-32t-1m-std        time:   [379.11 us]

Send 1,000 messages to 32 threads, then send 1,000 messages back from each
------------------------------------------------------------------
[#3] hydra-32t-1000m-flume     time:   [6.1528 ms]
[#1] hydra-32t-1000m-crossbeam time:   [3.1695 ms]
[#2] hydra-32t-1000m-std       time:   [4.3263 ms]

Send 1,000 messages to 1 thread, then send 1,000 messages back
------------------------------------------------------------------
[#1] hydra-1t-1000m-flume      time:   [103.93 us]
[#2] hydra-1t-1000m-crossbeam  time:   [114.26 us]
[#3] hydra-1t-1000m-std        time:   [254.15 us]

Send 10,000 messages to 4 threads, then send 10,000 messages back from each
------------------------------------------------------------------
[#2] hydra-4t-10000m-flume     time:   [4.1865 ms]
[#1] hydra-4t-10000m-crossbeam time:   [3.0606 ms]
[#3] hydra-4t-10000m-std       time:   [4.5998 ms]
```

## License

Flume is licensed under either of:

- Apache License 2.0, (http://www.apache.org/licenses/LICENSE-2.0)

- MIT license (http://opensource.org/licenses/MIT)
