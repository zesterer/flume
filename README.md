# Flume

A blazingly fast multi-producer, single-consumer channel.

[![Crates.io](https://img.shields.io/crates/v/flume.svg)](https://crates.io/crates/flume)
[Documentation](https://docs.rs/flume)

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

## Usage

To use Flume, place the following line under the `[dependencies]` section in your `Cargo.toml`:

```
flume = "x.y"
```

## Safety

Flume has no `unsafe` code, so you can be sure that it's not going to leave you with [nasal demons](http://catb.org/jargon/html/N/nasal-demons.html).

## Simplicity

Flume is implemented in just a few hundred lines of code and has very few dependencies. This makes it extremely fast to compile.

## Performance

Flume is considerably faster than `std`'s `mpsc` and `crossbeam_channel`.

The following benchmarks were performed on a quad-core 2.8 GHz Ryzen 7 CPU.

```
Create a channel
----------------
[#2] create-flume            time:   [124.79 ns]
[#3] create-crossbeam        time:   [186.35 ns]
[#1] create-std              time:   [49.873 ns] <--- Fastest

Create a channel, then send/recv a single message on it
-------------------------------------------------------
[#2] oneshot-flume           time:   [147.70 ns]
[#3] oneshot-crossbeam       time:   [301.21 ns]
[#1] oneshot-std             time:   [67.304 ns] <--- Fastest

Send/recv single messages on an existing channel
------------------------------------------------
[#1] inout-flume             time:   [17.353 ns] <--- Fastest
[#3] inout-crossbeam         time:   [53.441 ns]
[#2] inout-std               time:   [42.886 ns]

Send 1 message to 32 threads, then send 1 message back from each
------------------------------------------------------------------
[#1] hydra-32t-1m-flume      time:   [958.22 us] <--- Fastest
[#3] hydra-32t-1m-crossbeam  time:   [1.0062 ms]
[#2] hydra-32t-1m-std        time:   [960.67 us]

Send 1,000 messages to 32 threads, then send 1,000 messages back from each
------------------------------------------------------------------
[#3] hydra-32t-1000m-flume     time:   [1.7027 ms] <--- Fastest
[#1] hydra-32t-1000m-crossbeam time:   [2.7643 ms]
[#2] hydra-32t-1000m-std       time:   [6.1206 ms]

Send 1,000 messages to 1 thread, then send 1,000 messages back
------------------------------------------------------------------
[#1] hydra-1t-1000m-flume      time:   [111.09 us] <--- Fastest
[#2] hydra-1t-1000m-crossbeam  time:   [167.49 us]
[#3] hydra-1t-1000m-std        time:   [269.86 us]

Send 10,000 messages to 4 threads, then send 10,000 messages back from each
------------------------------------------------------------------
[#2] hydra-4t-10000m-flume     time:   [1.9595 ms] <--- Fastest
[#1] hydra-4t-10000m-crossbeam time:   [3.7293 ms]
[#3] hydra-4t-10000m-std       time:   [7.3757 ms]
```

## License

Flume is licensed under either of:

- Apache License 2.0, (http://www.apache.org/licenses/LICENSE-2.0)

- MIT license (http://opensource.org/licenses/MIT)
