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

Note: Currently, Windows is **not** supported.

See the [docs](https://docs.rs/local-runtime/latest) for more info.
