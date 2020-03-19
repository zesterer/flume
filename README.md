# Flume

A blazingly fast multi-producer, single-consumer channel.

[![Cargo](https://img.shields.io/crates/v/flume.svg)](
https://crates.io/crates/flume)
[![Documentation](https://docs.rs/flume/badge.svg)](
https://docs.rs/flume)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](
https://github.com/zesterer/flume)

```rust
let (tx, rx) = flume::unbounded();

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

## Features

- Bounded/unbounded queues
- No unsafe code
- Simple design, few dependencies, very fast to compile
- `async` support
- `select`-like interface (see [examples/select.rs](examples/select.rs))
- Feature parity with `std::sync::mpsc`

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

Flume is considerably faster than `std`'s `mpsc` and often outperforms `crossbeam_channel`.

The following benchmarks were performed on a quad-core (8 logical cores) 2.8 GHz Ryzen 7 CPU running the standard Arch Linux kernel.

```
Create a channel
----------------
create-flume            time:   [124.53 ns]
create-crossbeam        time:   [175.76 ns]
create-std              time:   [47.773 ns] <--- Fastest

Create a channel, then send/recv a single message on it
-------------------------------------------------------
oneshot-flume           time:   [147.49 ns]
oneshot-crossbeam       time:   [326.60 ns]
oneshot-std             time:   [64.348 ns] <--- Fastest

Send/recv single messages on an existing channel
------------------------------------------------
inout-flume             time:   [18.123 ns] <--- Fastest
inout-crossbeam         time:   [56.132 ns]
inout-std               time:   [42.288 ns]

Send 1 message to 32 threads, then send 1 message back from each
------------------------------------------------------------------
hydra-32t-1m-flume      time:   [98.227 us] <--- Fastest
hydra-32t-1m-crossbeam  time:   [423.58 us]
hydra-32t-1m-std        time:   [102.00 us]

Send 1,000 messages to 32 threads, then send 1,000 messages back from each
------------------------------------------------------------------
hydra-32t-1000m-flume     time:   [1.6267 ms] <--- Fastest
hydra-32t-1000m-crossbeam time:   [3.6800 ms]
hydra-32t-1000m-std       time:   [3.2338 ms]

Send 1,000 messages to 1 thread, then send 1,000 messages back
------------------------------------------------------------------
hydra-1t-1000m-flume      time:   [89.608 us]
hydra-1t-1000m-crossbeam  time:   [71.650 us] <--- Fastest
hydra-1t-1000m-std        time:   [180.53 us]

Send 10,000 messages to 4 threads, then send 10,000 messages back from each
------------------------------------------------------------------
hydra-4t-10000m-flume     time:   [2.7487 ms] <--- Fastest
hydra-4t-10000m-crossbeam time:   [3.2055 ms]
hydra-4t-10000m-std       time:   [4.8473 ms]
```

## License

Flume is licensed under either of:

- Apache License 2.0, (http://www.apache.org/licenses/LICENSE-2.0)

- MIT license (http://opensource.org/licenses/MIT)
