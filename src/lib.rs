//! Thread-local async runtime
//!
//! This crate provides an async runtime that runs entirely within the current thread. As such, it
//! can run futures that are `!Send` and non-`static`. In addition, it also provides timers and
//! an async wrapper for standard I/O types, similar to [`async-io`](https://docs.rs/async-io/latest/async_io/index.html).
//!
//! The entry point of the runtime is [`block_on`], which drives a future to completion on the
//! current thread.
//!
//! # Implementation
//!
//! Task wakeups are handled by a thread-local reactor, which keeps track of all I/O events and
//! timers in the current thread along with their associated wakers. Waiting for the reactor is
//! done by [`block_on`], without needing a separate thread.
//!
//! The implementation of the reactor depends on the platform. On Unix systems, the reactor uses
//! [`poll`](https://pubs.opengroup.org/onlinepubs/9699919799/functions/poll.html). Currently,
//! Windows is not supported.
//!
//! # Concurrent tasks
//!
//! Unlike other async executors, this crate doesn't have an API for spawning tasks. Instead, use
//! the [`join!`] or [`merge_futures`] macro to run multiple tasks concurrently.
//!
//! # Examples
//!
//! Concurrently listen for connections on a local port while making connections to localhost with
//! a 500 microsecond delay. Return with error if any operation fails.
//!
//! ```no_run
//! use std::net::{TcpStream, TcpListener};
//! use std::time::Duration;
//! use std::io;
//! use std::pin::pin;
//!
//! use futures_lite::{AsyncReadExt, AsyncWriteExt, StreamExt};
//! use local_runtime::{io::Async, time::sleep, block_on, merge_futures};
//!
//! # fn main() -> std::io::Result<()> {
//! let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
//! let addr = listener.get_ref().local_addr()?;
//!
//! block_on(async {
//!     let task1 = pin!(async {
//!         loop {
//!             let (mut stream, _) = listener.accept().await?;
//!             let mut buf = [0u8; 5];
//!             stream.read_exact(&mut buf).await?;
//!             assert_eq!(&buf, b"hello");
//!         }
//!         Ok::<_, io::Error>(())
//!     });
//!
//!     let task2 = pin!(async {
//!         loop {
//!             let mut stream = Async::<TcpStream>::connect(addr).await?;
//!             stream.write_all(b"hello").await?;
//!             sleep(Duration::from_micros(500)).await;
//!         }
//!         Ok::<_, io::Error>(())
//!     });
//!
//!     // Process the result of each task as a stream, returning early on error
//!     merge_futures!(task1, task2).try_for_each(|x| x).await
//! })?;
//! # Ok(())
//! # }
//! ```

pub mod io;
mod join;
mod reactor;
#[cfg(test)]
mod test;
pub mod time;

use std::{
    future::Future,
    num::NonZero,
    pin::pin,
    sync::Weak,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use reactor::{Notifier, NotifierImpl, Reactor, REACTOR};
use time::TIMER_QUEUE;

pub use join::{JoinFuture, MergeFutureStream, MergeStream};

// Option<Id> will be same size as `usize`
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq, Hash)]
struct Id(NonZero<usize>);

impl Id {
    const fn new(n: usize) -> Self {
        Id(NonZero::new(n).expect("expected non-zero ID"))
    }

    const fn overflowing_incr(&self) -> Self {
        // Wrap back around to 1 on overflow
        match self.0.checked_add(1) {
            Some(next) => Self(next),
            None => const { Id::new(1) },
        }
    }
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, wake, wake_by_ref, waker_drop);

// Use a weak pointer to the reactor's notifier as the waker, which will wake up the reactor when
// it's waiting.
fn create_waker(notifier: Weak<NotifierImpl>) -> Waker {
    let raw = RawWaker::new(notifier.into_raw() as *const (), &WAKER_VTABLE);
    // SAFETY: WAKER_VTABLE follows all safety guarantees
    unsafe { Waker::from_raw(raw) }
}

unsafe fn waker_clone(ptr: *const ()) -> RawWaker {
    let weak = Weak::from_raw(ptr as *const NotifierImpl);
    let clone = Weak::clone(&weak);
    std::mem::forget(weak);
    RawWaker::new(clone.into_raw() as *const (), &WAKER_VTABLE)
}

unsafe fn waker_drop(ptr: *const ()) {
    drop(Weak::from_raw(ptr as *const NotifierImpl));
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let weak = Weak::from_raw(ptr as *const NotifierImpl);
    if let Some(arc) = weak.upgrade() {
        let _ = arc.notify();
    }
    std::mem::forget(weak);
}

unsafe fn wake(ptr: *const ()) {
    wake_by_ref(ptr);
    waker_drop(ptr);
}

/// Drives a future to completion on the current thread, processing I/O events when idle.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use local_runtime::time::Timer;
///
/// local_runtime::block_on(async {
///     Timer::delay(Duration::from_millis(10)).await;
/// });
/// ```
pub fn block_on<T, F>(mut fut: F) -> T
where
    F: Future<Output = T>,
{
    let mut fut = pin!(fut);
    let waker = create_waker(REACTOR.with(|r| r.notifier()));
    REACTOR.with(|r| r.clear_notifications());

    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(&waker)) {
            return out;
        }

        let wait_res = TIMER_QUEUE.with(|tq| REACTOR.with(|r| r.wait(tq)));
        if let Err(err) = wait_res {
            log::error!(
                "{:?} Error polling reactor: {err}",
                std::thread::current().id()
            );
        }
    }
}
