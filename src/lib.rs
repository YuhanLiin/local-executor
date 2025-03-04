#![warn(missing_docs)]

//! Thread-local async runtime
//!
//! This crate provides an async runtime that runs entirely within the current thread. As such, it
//! can run futures that are `!Send` and non-`static`. If no future is able to make progress, the
//! runtime will suspend the current thread until a future is ready to be polled.
//!
//! To actually run a future, see [`block_on`] or [`Executor::block_on`], which drives the future
//! to completion on the current thread.
//!
//! In addition, This crate provides [async timers](crate::time) and an [async adapter](Async)
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
//! # Concurrency
//!
//! The [`Executor`] can spawn tasks that run concurrently on the same thread. Alternatively, this
//! crate provides macros such as [`join`] and [`merge_futures`] for concurrent execution.
//!
//! # Compatibility
//!
//! Unlike other runtimes, `local_runtime` doesn't run the reactor in the background, instead
//! relying on [`block_on`] to run the reactor while polling the future. Since leaf futures from
//! this crate, such as [`Async`] and timers, rely on the reactor to wake up, **they can only be
//! driven by [`block_on`], and are not compatible with other runtimes**.
//!
//! # Examples
//!
//! Listen for connections on a local port, while concurrently making connections to localhost.
//! Return with error if any operation fails.
//!
//! ```no_run
//! use std::{net::{TcpStream, TcpListener}, time::Duration, io};
//! use futures_lite::{AsyncReadExt, AsyncWriteExt, StreamExt};
//! use local_runtime::{io::Async, time::sleep, Executor};
//!
//! # fn main() -> std::io::Result<()> {
//! let ex = Executor::new();
//! ex.block_on(async {
//!     let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
//!     let addr = listener.get_ref().local_addr()?;
//!
//!     // Run this task in the background
//!     let _bg = ex.spawn(async move {
//!         // Listen for connections on local port
//!         loop {
//!             let (mut stream, _) = listener.accept().await?;
//!             let mut buf = [0u8; 5];
//!             stream.read_exact(&mut buf).await?;
//!             assert_eq!(&buf, b"hello");
//!         }
//!         Ok::<_, io::Error>(())
//!     });
//!
//!     // Connect to the listener repeatedly with 50us delay
//!     loop {
//!         let mut stream = Async::<TcpStream>::connect(addr).await?;
//!         stream.write_all(b"hello").await?;
//!         sleep(Duration::from_micros(500)).await;
//!     }
//!     Ok::<_, io::Error>(())
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
    cell::{Cell, RefCell},
    future::{poll_fn, Future},
    num::NonZero,
    pin::{pin, Pin},
    rc::Rc,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
};

use concurrency::FlagWaker;
use futures_core::future::LocalBoxFuture;
use reactor::{Notifier, REACTOR};

#[doc(hidden)]
pub use concurrency::{JoinFuture, MergeFutureStream, MergeStream};
pub use io::Async;
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

impl Wake for Notifier {
    fn wake(self: Arc<Self>) {
        let _ = self.notify();
    }
}

/// Drives a future to completion on the current thread, processing I/O events when idle.
///
/// Does not support task spawning (see [`Executor::run`]).
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
    let waker = REACTOR.with(|r| r.notifier()).into();

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
    waker_pair: (Arc<FlagWaker>, Waker),
    cancelled: Rc<Cell<bool>>,
}

impl Task<'_> {
    fn poll(&mut self, base_waker: &Waker) -> Poll<()> {
        let (waker_data, waker) = &self.waker_pair;
        if waker_data.check_awoken(base_waker) {
            self.future.as_mut().poll(&mut Context::from_waker(waker))
        } else {
            Poll::Pending
        }
    }
}

/// An async executor that can spawn tasks
///
/// # Example
///
/// Run a future that spawns tasks and captures the outside environment.
///
/// ```
/// use local_runtime::{block_on, Executor};
///
/// // Run future on current thread
/// block_on(async {
///     let n = 10;
///     let ex = Executor::new();
///     let out = ex.run(async {
///         // Spawn an async task that captures from the outside environment
///         let handle = ex.spawn(async { &n });
///         // Wait for the task to complete
///         handle.await
///     }).await;
///     assert_eq!(*out, 10);
/// });
/// ```
pub struct Executor<'a> {
    tasks: RefCell<Slab<Task<'a>>>,
    spawned: RefCell<Vec<Task<'a>>>,
}

impl Default for Executor<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Executor<'a> {
    /// Create new executor
    pub fn new() -> Self {
        Self {
            tasks: RefCell::new(Slab::with_capacity(8)),
            spawned: RefCell::new(Vec::with_capacity(8)),
        }
    }

    /// Spawn a task on the executor, returning a [`TaskHandle`] to it
    ///
    /// The provided future will run concurrently on the current thread while [`Executor::run`]
    /// runs, even if you don't await on the `TaskHandle`. If it's not awaited, there's no
    /// guarantee that the task will run to completion.
    ///
    /// To spawn additional tasks from inside of a spawned task, see [`Executor::spawn_rc`].
    ///
    /// ```no_run
    /// use std::net::TcpListener;
    /// use local_runtime::{io::Async, Executor, block_on};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let ex = Executor::new();
    /// block_on(ex.run(async {
    ///     let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 8080))?;
    ///     loop {
    ///         let mut stream = listener.accept().await?;
    ///         let task = ex.spawn(async move {
    ///             // Process each connection concurrently
    ///         });
    ///     }
    ///     Ok(())
    /// }))
    /// # }
    /// ```
    pub fn spawn<T: 'a>(&self, fut: impl Future<Output = T> + 'a) -> TaskHandle<T> {
        let ret = Rc::new(RefCell::new(RetState {
            value: None,
            waker: None,
        }));
        let ret_clone = ret.clone();
        let cancelled = Rc::new(Cell::new(false));

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
            waker_pair: FlagWaker::waker_pair(),
            cancelled: cancelled.clone(),
        });
        TaskHandle { ret, cancelled }
    }

    /// Spawn a task using a [`Rc`] pointer to the executor, rather than a reference. This allows
    /// for spawning more tasks inside spawned tasks.
    ///
    /// When attempting "recursive" task spawning using [`Executor::spawn`], you will encounter
    /// borrow checker errors about the lifetime of the executor:
    ///
    /// ```compile_fail
    /// use local_runtime::Executor;
    ///
    /// let ex = Executor::new();
    /// //  -- binding `ex` declared here
    /// ex.block_on(async {
    ///     //      ----- value captured here by coroutine
    ///     let outer_task = ex.spawn(async {
    ///     //               ^^ borrowed value does not live long enough
    ///         let inner_task = ex.spawn(async { 10 });
    ///         inner_task.await;
    ///     });
    /// });
    /// // -
    /// // |
    /// // `ex` dropped here while still borrowed
    /// // borrow might be used here, when `ex` is dropped and runs the destructor for type `Executor<'_>`
    /// ```
    ///
    /// This happens because the future associated with the task is stored in the executor. So if
    /// `outer_task` contains a reference to the executor, then the executor will be storing a
    /// reference to itself, which is not allowed. To circumvent this issue, we need to put the
    /// executor behind a [`Rc`] pointer and clone it into every task that we want to spawn more
    /// tasks in. This is where [`Executor::spawn_rc`] comes in.
    ///
    /// Rather than taking a future, `spawn_rc` accepts a closure that takes a `Rc` to executor
    /// and returns a future. This allows the future to capture the executor by value rather than
    /// by reference, getting rid of the borrow error.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::rc::Rc;
    /// use local_runtime::Executor;
    ///
    /// let ex = Rc::new(Executor::new());
    /// ex.block_on(async {
    ///     let outer_task = ex.clone().spawn_rc(|ex| async move {
    ///         let inner_task = ex.spawn(async { 10 });
    ///         inner_task.await;
    ///     });
    /// });
    /// ```
    pub fn spawn_rc<T: 'a, Fut: Future<Output = T> + 'a, F>(self: Rc<Self>, f: F) -> TaskHandle<T>
    where
        F: FnOnce(Rc<Self>) -> Fut + 'a,
    {
        let cl = self.clone();
        self.spawn(f(cl))
    }

    fn poll_tasks(&self, base_waker: &Waker) {
        let mut tasks = self.tasks.borrow_mut();
        // Check existing tasks
        for i in 0..tasks.capacity() {
            if let Some(task) = tasks.get_mut(i) {
                // If a task is cancelled, don't poll it, just remove it
                if task.cancelled.get() || task.poll(base_waker).is_ready() {
                    tasks.remove(i);
                }
            }
        }

        // Keep checking newly spawend tasks until there's no more left
        // Reborrow the spawned tasks on every iteration, because the tasks themselves also need to
        // borrow the spawned tasks
        while let Some(mut spawned_task) = self.spawned.borrow_mut().pop() {
            // Only poll and insert non-cancelled tasks
            if !spawned_task.cancelled.get() && spawned_task.poll(base_waker).is_pending() {
                tasks.insert(spawned_task);
            }
        }

        log::trace!(
            "{:?} {} tasks in executor",
            std::thread::current().id(),
            tasks.len()
        );
    }

    /// Blocking version of [`Executor::run`].
    ///
    /// This is just a shorthand for calling `block_on(ex.run(fut))`.
    ///
    /// # Panic
    ///
    /// Calling this function within a task spawned on the same executor will panic.
    pub fn block_on<T>(&self, fut: impl Future<Output = T>) -> T {
        block_on(self.run(fut))
    }

    /// Drives the future to completion asynchronously while also driving all spawned tasks
    ///
    /// When this function completes, it will drop all unfinished tasks that were spawned on the
    /// executor.
    ///
    /// # Panic
    ///
    /// Polling the future returned by this function within a task spawned on the same executor will
    /// panic.
    ///
    /// # Example
    ///
    /// ```
    /// use std::net::UdpSocket;
    /// use std::io;
    /// use local_runtime::{block_on, Executor, Async};
    ///
    /// // Run future on current thread
    /// block_on(async {
    ///     let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0))?;
    ///     let addr = socket.get_ref().local_addr()?;
    ///     socket.connect(addr)?;
    ///
    ///     let ex = Executor::new();
    ///     ex.run(async {
    ///         let task = ex.spawn(async {
    ///             socket.send(b"hello").await?;
    ///             socket.send(b"hello").await?;
    ///             Ok::<_, io::Error>(())
    ///         });
    ///
    ///         let mut data = [0u8; 5];
    ///         socket.recv(&mut data).await?;
    ///         socket.recv(&mut data).await?;
    ///         task.await
    ///     }).await
    /// });
    /// ```
    pub async fn run<T>(&self, fut: impl Future<Output = T>) -> T {
        let mut fut = pin!(fut);
        // Create waker for main future
        let (main_waker_data, main_waker) = FlagWaker::waker_pair();

        let out = poll_fn(move |cx| {
            if main_waker_data.check_awoken(cx.waker()) {
                if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(&main_waker)) {
                    return Poll::Ready(out);
                }
            }
            self.poll_tasks(cx.waker());
            Poll::Pending
        })
        .await;

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

/// A handle to a spawned task
///
/// A `TaskHandle` can be awaited to wait for the completion of its associated task and get its
/// result.
///
/// A `TaskHandle` detaches its task when dropped. This means the it can no longer be awaited, but
/// the executor will still poll its task.
///
/// This is created by [`Executor::spawn`] and [`Executor::spawn_rc`].
#[derive(Debug)]
pub struct TaskHandle<T> {
    ret: Rc<RefCell<RetState<T>>>,
    cancelled: Rc<Cell<bool>>,
}

impl<T> TaskHandle<T> {
    /// Cancel the task
    ///
    /// Deletes the task from the executor so that it won't be polled again.
    ///
    /// If the handle is awaited after cancellation, it might still complete if the task was
    /// already finished before it was cancelled. However, the likelier outcomes is that it never
    /// completes.
    pub fn cancel(&self) {
        self.cancelled.set(true);
    }

    /// Check if this task is finished
    ///
    /// If this returns `true`, the next `poll` call is guaranteed to return [`Poll::Ready`].
    pub fn is_finished(&self) -> bool {
        self.ret.borrow().value.is_some()
    }

    /// Check if this task has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }
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

    use crate::test::MockWaker;

    use super::*;

    #[test]
    fn spawn_and_poll() {
        let waker = Arc::new(MockWaker::default());
        let ex = Executor::new();
        assert_eq!(ex.tasks.borrow().len(), 0);

        ex.spawn(pending::<()>());
        ex.spawn(pending::<()>());
        ex.spawn(pending::<()>());
        ex.poll_tasks(&waker.clone().into());
        assert_eq!(ex.tasks.borrow().len(), 3);

        ex.spawn(async {});
        ex.spawn(async {});
        ex.poll_tasks(&waker.clone().into());
        assert_eq!(ex.tasks.borrow().len(), 3);
    }

    #[test]
    fn cancel() {
        let waker = Arc::new(MockWaker::default());
        let ex = Executor::new();
        assert_eq!(ex.tasks.borrow().len(), 0);

        let task = ex.spawn(pending::<()>());
        // Cancel task while it's in the spawned list
        task.cancel();
        assert!(task.is_cancelled());
        ex.poll_tasks(&waker.clone().into());
        assert_eq!(ex.tasks.borrow().len(), 0);

        let task = ex.spawn(pending::<()>());
        assert!(!task.is_cancelled());
        ex.poll_tasks(&waker.clone().into());
        assert_eq!(ex.tasks.borrow().len(), 1);

        // Cancel task while it's in the task list
        task.cancel();
        ex.poll_tasks(&waker.clone().into());
        assert_eq!(ex.tasks.borrow().len(), 0);
    }
}
