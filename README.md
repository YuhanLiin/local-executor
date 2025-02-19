![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/YuhanLiin/local-runtime/ci.yml)
[![docs.rs](https://img.shields.io/docsrs/local-runtime)](https://docs.rs/local-runtime/latest)
[![Crates.io Version](https://img.shields.io/crates/v/local-runtime)](https://crates.io/crates/local-runtime)

# local-runtime

Thread-local async runtime

This crate provides an async runtime that runs entirely within the current thread. As such, it
can run futures that are `!Send` and non-`static`. If no future is able to make progress, the
runtime will suspend the current thread until a future is ready to be polled.

In addition, This crate provides async timers and an async adapter for standard
I/O types, similar to
[`async-io`](https://docs.rs/async-io/latest/async_io/index.html).

## Example

Concurrently listen for connections on a local port while making connections to localhost with
a 500 microsecond delay. Return with error if any operation fails.

```rust
use std::net::{TcpStream, TcpListener};
use std::time::Duration;
use std::io;
use std::pin::pin;
use futures_lite::{AsyncReadExt, AsyncWriteExt, StreamExt};
use local_runtime::{io::Async, time::sleep, block_on, merge_futures};

let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
let addr = listener.get_ref().local_addr()?;

block_on(async {
    let task1 = pin!(async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"hello");
        }
        Ok::<_, io::Error>(())
    });

    let task2 = pin!(async {
        loop {
            let mut stream = Async::<TcpStream>::connect(addr).await?;
            stream.write_all(b"hello").await?;
            sleep(Duration::from_micros(500)).await;
        }
        Ok::<_, io::Error>(())
    });

    // Process the result of each task as a stream, returning early on error
    merge_futures!(task1, task2).try_for_each(|x| x).await
})?;
```
