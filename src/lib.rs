#![warn(missing_docs)]

//! Thread-local async runtime
//!
//! This crate provides an async runtime that runs entirely within the current thread. As such, it
//! can run futures that are `!Send` and non-`static`. If no future is able to make progress, the
//! runtime will suspend the current thread until a future is ready to be polled.
//!
//! The entry point of the runtime is [`block_on`], which drives a future to completion on the
//! current thread.
//!
//! In addition, This crate provides [async timers](crate::time) and an [async adapter](crate::io)
//! for standard I/O types, similar to
//! [`async-io`](https://docs.rs/async-io/latest/async_io/index.html).
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

mod concurrency;
pub mod io;
mod reactor;
#[cfg(test)]
mod test;
pub mod time;

use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    num::NonZero,
    pin::{self, pin, Pin},
    rc::Rc,
    sync::{Arc, Weak},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use concurrency::FlagWaker;
use futures_core::future::LocalBoxFuture;
use reactor::{Notifier, REACTOR};

#[doc(hidden)]
pub use concurrency::{JoinFuture, MergeFutureStream, MergeStream};
use slab::Slab;

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
fn create_waker(notifier: Weak<Notifier>) -> Waker {
    let raw = RawWaker::new(notifier.into_raw() as *const (), &WAKER_VTABLE);
    // SAFETY: WAKER_VTABLE follows all safety guarantees
    unsafe { Waker::from_raw(raw) }
}

unsafe fn waker_clone(ptr: *const ()) -> RawWaker {
    let weak = Weak::from_raw(ptr as *const Notifier);
    let clone = Weak::clone(&weak);
    std::mem::forget(weak);
    RawWaker::new(clone.into_raw() as *const (), &WAKER_VTABLE)
}

unsafe fn waker_drop(ptr: *const ()) {
    drop(Weak::from_raw(ptr as *const Notifier));
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let weak = Weak::from_raw(ptr as *const Notifier);
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

        let wait_res = REACTOR.with(|r| r.wait());
        if let Err(err) = wait_res {
            log::error!(
                "{:?} Error polling reactor: {err}",
                std::thread::current().id()
            );
        }
    }
}

struct Task<'a> {
    future: LocalBoxFuture<'a, ()>,
    waker_data: Arc<FlagWaker>,
    waker: Waker,
}

impl Task<'_> {
    fn poll(&mut self) -> Poll<()> {
        if self.waker_data.check_awoken() {
            self.future
                .as_mut()
                .poll(&mut Context::from_waker(&self.waker))
        } else {
            Poll::Pending
        }
    }
}

pub struct Executor<'a> {
    tasks: RefCell<Slab<Task<'a>>>,
    spawned: RefCell<Vec<Task<'a>>>,
    base_waker: Waker,
}

impl<'a> Executor<'a> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            tasks: RefCell::new(Slab::with_capacity(8)),
            spawned: RefCell::new(Vec::with_capacity(8)),
            base_waker: create_waker(REACTOR.with(|r| r.notifier())),
        }
    }

    pub fn spawn<T: 'a>(&self, fut: impl Future<Output = T> + 'a) -> TaskHandle<T> {
        let ret = Rc::new(RefCell::new(RetState {
            value: None,
            waker: None,
        }));
        let ret_clone = ret.clone();

        let waker_data = Arc::new(FlagWaker::from(self.base_waker.clone()));
        let waker = waker_data.clone().into();

        let mut spawned = self.spawned.borrow_mut();
        spawned.push(Task {
            future: Box::pin(async move {
                let retval = fut.await;
                let mut ret = ret_clone.borrow_mut();
                ret.value = Some(retval);
                if let Some(waker) = &ret.waker {
                    waker.wake_by_ref();
                }
            }),
            waker_data,
            waker,
        });
        TaskHandle { ret }
    }

    pub fn spawn_rc<T: 'a, Fut: Future<Output = T> + 'a, F>(self: Rc<Self>, f: F) -> TaskHandle<T>
    where
        F: FnOnce(Rc<Self>) -> Fut + 'a,
    {
        let cl = self.clone();
        self.spawn(f(cl))
    }

    fn poll_tasks(&self) {
        let mut tasks = self.tasks.borrow_mut();
        for i in 0..tasks.capacity() {
            if let Some(task) = tasks.get_mut(i) {
                if task.poll().is_ready() {
                    tasks.remove(i);
                }
            }
        }
        // Reborrow the spawned tasks on every iteration, because the tasks themselves also need to
        // borrow the spawned tasks
        while let Some(mut spawed_task) = self.spawned.borrow_mut().pop() {
            if spawed_task.poll().is_pending() {
                tasks.insert(spawed_task);
            }
        }
    }

    pub fn run<T>(&self, mut fut: impl Future<Output = T>) -> T {
        let mut fut = pin!(fut);
        REACTOR.with(|r| r.clear_notifications());

        // Create waker for main future
        let main_waker_data = Arc::new(FlagWaker::from(self.base_waker.clone()));
        let main_waker = main_waker_data.clone().into();

        let out = loop {
            if main_waker_data.check_awoken() {
                if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(&main_waker)) {
                    break out;
                }
            }
            self.poll_tasks();

            let wait_res = REACTOR.with(|r| r.wait());
            if let Err(err) = wait_res {
                log::error!(
                    "{:?} Error polling reactor: {err}",
                    std::thread::current().id()
                );
            }
        };

        // Drop all unfinished tasks so that any Rc<Executor> inside the tasks are dropped. This
        // prevents Rc-cycles and guarantees that the executor will be dropped later
        self.tasks.borrow_mut().clear();
        self.spawned.borrow_mut().clear();
        out
    }
}

#[derive(Debug)]
struct RetState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

#[derive(Debug)]
pub struct TaskHandle<T> {
    ret: Rc<RefCell<RetState<T>>>,
}

impl<T> Future for TaskHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut ret = self.ret.borrow_mut();
        if let Some(val) = ret.value.take() {
            return Poll::Ready(val);
        }

        match &mut ret.waker {
            Some(waker) => waker.clone_from(cx.waker()),
            None => ret.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    #[test]
    fn spawn_and_poll() {
        let ex = Executor::new();
        assert_eq!(ex.tasks.borrow().len(), 0);

        ex.spawn(pending::<()>());
        ex.spawn(pending::<()>());
        ex.spawn(pending::<()>());
        ex.poll_tasks();
        assert_eq!(ex.tasks.borrow().len(), 3);

        ex.spawn(async {});
        ex.spawn(async {});
        ex.poll_tasks();
        assert_eq!(ex.tasks.borrow().len(), 3);
    }
}
