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
    cell::{Cell, RefCell, UnsafeCell},
    collections::VecDeque,
    fmt::Debug,
    future::{poll_fn, Future},
    num::NonZero,
    pin::{pin, Pin},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, ThreadId},
};

use atomic_waker::AtomicWaker;
use concurrent_queue::ConcurrentQueue;
use futures_core::future::LocalBoxFuture;
use slab::Slab;

#[doc(hidden)]
pub use concurrency::{JoinFuture, MergeFutureStream, MergeStream};
pub use io::Async;
use reactor::{Notifier, REACTOR};

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

#[derive(Debug)]
struct WakeQueue {
    base_waker: AtomicWaker,
    local_thread: ThreadId,
    local: UnsafeCell<VecDeque<usize>>,
    concurrent: ConcurrentQueue<usize>,
}

// SAFETY: The thread-unsafety comes from `local`, which will only be accessed if the current
// thread equals `local_thread`. As such, `local` will never be accessed from multiple threads.
unsafe impl Send for WakeQueue {}
unsafe impl Sync for WakeQueue {}

impl WakeQueue {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            base_waker: AtomicWaker::new(),
            local_thread: thread::current().id(),
            local: UnsafeCell::new(VecDeque::with_capacity(capacity)),
            concurrent: ConcurrentQueue::unbounded(),
        }
    }

    fn push(&self, val: usize) {
        if thread::current().id() == self.local_thread {
            // SAFETY: Like all other accesses to `local`, this access can only happen if current
            // thread is `local_thread`, and also has limited lifetime. As such, this access will
            // never cause a data race or collide with any other access of `local`.
            unsafe { (*self.local.get()).push_back(val) };
        } else {
            // If queue is closed, then just don't do anything
            let _ = self.concurrent.push(val);
        }
    }

    fn drain_for_each<F: FnMut(usize)>(&self, mut f: F) {
        if thread::current().id() == self.local_thread {
            // SAFETY: Like all other accesses to `local`, this access can only happen if current
            // thread is `local_thread`, and also has limited lifetime. As such, this access will
            // never cause a data race or collide with any other access of `local`.
            let local_len = unsafe { (*self.local.get()).len() };
            let con_len = self.concurrent.len();

            log::trace!(
                "{:?} {local_len} local wakeups, {con_len} concurrent wakeups",
                std::thread::current().id()
            );

            // Set upperbounds for the iteration on the two queues to ensure we never loop forever
            // if the callback also adds values to the queue
            for _ in 0..local_len {
                let val = unsafe { (*self.local.get()).pop_front().unwrap() };
                f(val);
            }
            for val in self.concurrent.try_iter().take(con_len) {
                f(val);
            }
        }
    }

    fn reset(&self, init_val: usize) {
        if thread::current().id() == self.local_thread {
            // SAFETY: Like all other accesses to `local`, this access can only happen if current
            // thread is `local_thread`, and also has limited lifetime. As such, this access will
            // never cause a data race or collide with any other access of `local`.
            unsafe {
                (*self.local.get()).clear();
                (*self.local.get()).push_back(init_val);
            }
            // Pop all remaining elements
            while self.concurrent.pop().is_ok() {}
        }
    }
}

struct TaskWaker {
    queue: Arc<WakeQueue>,
    awoken: AtomicBool,
    task_id: usize,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        // Ensure that we only push the task ID to the queue once per wakeup
        // Use relaxed memory ordering here AtomicWaker already provides strict memory ordering
        if self
            .awoken
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.queue.push(self.task_id);
            // Release memory ordering
            self.queue.base_waker.wake();
        }
    }
}

impl TaskWaker {
    fn new(queue: Arc<WakeQueue>, task_id: usize) -> Self {
        Self {
            awoken: AtomicBool::new(false),
            queue,
            task_id,
        }
    }

    fn waker_pair(queue: Arc<WakeQueue>, task_id: usize) -> (Arc<Self>, Waker) {
        let this = Arc::new(Self::new(queue, task_id));
        let waker = this.clone().into();
        (this, waker)
    }

    fn to_sleep(&self) {
        self.awoken.store(false, Ordering::Relaxed);
    }
}

struct SpawnedTask<'a> {
    future: LocalBoxFuture<'a, ()>,
    handle_data: Rc<HandleData>,
}

struct Task<'a> {
    future: LocalBoxFuture<'a, ()>,
    handle_data: Rc<HandleData>,
    waker_pair: (Arc<TaskWaker>, Waker),
}

impl<'a> Task<'a> {
    fn poll(&mut self) -> Poll<()> {
        let (waker_data, waker) = &self.waker_pair;
        // Reset this waker so that it can produce wakeups again
        waker_data.to_sleep();
        self.future.as_mut().poll(&mut Context::from_waker(waker))
    }

    fn from_spawned(spawned_task: SpawnedTask<'a>, waker_pair: (Arc<TaskWaker>, Waker)) -> Self {
        let handle_data = spawned_task.handle_data;
        // Set up waker for the handle
        handle_data.waker.set(Some(waker_pair.1.clone()));
        Self {
            future: spawned_task.future,
            handle_data,
            waker_pair,
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
    spawned: RefCell<Vec<SpawnedTask<'a>>>,
    wake_queue: Arc<WakeQueue>,
}

impl Default for Executor<'_> {
    fn default() -> Self {
        Self::new()
    }
}

const MAIN_TASK_ID: usize = usize::MAX;

impl<'a> Executor<'a> {
    /// Create new executor
    pub fn new() -> Self {
        Self::with_capacity(4)
    }

    /// Create new executor with a pre-allocated capacity
    ///
    /// The executor will be able to hold at least `capacity` concurrent tasks without reallocating
    /// its internal storage.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            tasks: RefCell::new(Slab::with_capacity(capacity)),
            spawned: RefCell::new(Vec::with_capacity(capacity)),
            wake_queue: Arc::new(WakeQueue::with_capacity(capacity)),
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
        let ret = Rc::new(RetData {
            value: Cell::new(None),
            waker: Cell::new(None),
        });
        let ret_clone = ret.clone();
        let handle_data = Rc::<HandleData>::default();

        let mut spawned = self.spawned.borrow_mut();
        spawned.push(SpawnedTask {
            future: Box::pin(async move {
                let retval = fut.await;
                let ret = ret_clone;
                ret.value.set(Some(retval));
                if let Some(waker) = ret.waker.take() {
                    waker.wake();
                }
            }),
            handle_data: handle_data.clone(),
        });
        TaskHandle { ret, handle_data }
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

    fn register_base_waker(&self, base_waker: &Waker) {
        // Acquire ordering
        self.wake_queue.base_waker.register(base_waker);
    }

    // Poll tasks that have been awoken, returning whether the main future has been awoken
    fn poll_tasks(&self) -> bool {
        let mut main_task_awoken = false;
        let mut tasks = self.tasks.borrow_mut();

        self.wake_queue.drain_for_each(|task_id| {
            if task_id == MAIN_TASK_ID {
                main_task_awoken = true;
            }
            // For each awoken task, find it if it still exists
            else if let Some(task) = tasks.get_mut(task_id) {
                // If a task is cancelled, don't poll it, just remove it
                if task.handle_data.cancelled.get() || task.poll().is_ready() {
                    tasks.remove(task_id);
                }
            }
        });

        main_task_awoken
    }

    // Poll newly spawned tasks and move them to the task list
    fn poll_spawned(&self) {
        let mut tasks = self.tasks.borrow_mut();
        // Keep checking newly spawned tasks until there's no more left.
        // Reborrow the spawned tasks on every iteration, because the tasks themselves also need to
        // borrow the spawned tasks.
        while let Some(spawned_task) = self.spawned.borrow_mut().pop() {
            // Ignore cancelled tasks
            if spawned_task.handle_data.cancelled.get() {
                continue;
            }

            let next_vacancy = tasks.vacant_entry();
            // Use the Slab index as the task ID.
            // If the waker outlives the task or the task calls the waker even after returning
            // Ready, then it's possible for the waker to wake up another task that's replaced the
            // original task in the Slab. This should be rare, and at worse causes spurious wakeups.
            let task_id = next_vacancy.key();
            assert_ne!(task_id, MAIN_TASK_ID);
            let waker_pair = TaskWaker::waker_pair(self.wake_queue.clone(), task_id);
            let mut task = Task::from_spawned(spawned_task, waker_pair);
            // Only insert the task if it returns pending
            if task.poll().is_pending() {
                next_vacancy.insert(task);
            }
        }
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
        let (main_waker_data, main_waker) =
            TaskWaker::waker_pair(self.wake_queue.clone(), MAIN_TASK_ID);
        self.wake_queue.reset(MAIN_TASK_ID);

        let out = poll_fn(move |cx| {
            self.register_base_waker(cx.waker());
            let main_task_awoken = self.poll_tasks();
            if main_task_awoken {
                main_waker_data.to_sleep();
                if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(&main_waker)) {
                    return Poll::Ready(out);
                }
            }
            self.poll_spawned();
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

struct RetData<T> {
    value: Cell<Option<T>>,
    waker: Cell<Option<Waker>>,
}

#[derive(Default)]
struct HandleData {
    cancelled: Cell<bool>,
    waker: Cell<Option<Waker>>,
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
pub struct TaskHandle<T> {
    ret: Rc<RetData<T>>,
    handle_data: Rc<HandleData>,
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
        self.handle_data.cancelled.set(true);
        // If the task has a waker, then it has already been added to the task list, so it needs to
        // be awoken in order for its cancellation status to be checked
        if let Some(waker) = self.handle_data.waker.take() {
            waker.wake();
        }
    }

    /// Check if this task is finished
    ///
    /// If this returns `true`, the next `poll` call is guaranteed to return [`Poll::Ready`].
    pub fn is_finished(&self) -> bool {
        // SAFETY: We never get a long-lived reference to ret.value, so aliasing cannot occur
        unsafe { (*self.ret.value.as_ptr()).is_some() }
    }

    /// Check if this task has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.handle_data.cancelled.get()
    }
}

impl<T> Future for TaskHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(val) = self.ret.value.take() {
            return Poll::Ready(val);
        }

        let mut waker = self.ret.waker.take();
        match &mut waker {
            Some(waker) => waker.clone_from(cx.waker()),
            None => waker = Some(cx.waker().clone()),
        }
        self.ret.waker.set(waker);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        time::{Duration, Instant},
    };

    use crate::{test::MockWaker, time::sleep};

    use super::*;

    #[test]
    fn spawn_and_poll() {
        let ex = Executor::new();
        assert_eq!(ex.tasks.borrow().len(), 0);

        ex.spawn(pending::<()>());
        ex.spawn(pending::<()>());
        ex.spawn(pending::<()>());
        ex.poll_tasks();
        ex.poll_spawned();
        assert_eq!(ex.tasks.borrow().len(), 3);

        ex.spawn(async {});
        ex.spawn(async {});
        ex.poll_tasks();
        ex.poll_spawned();
        assert_eq!(ex.tasks.borrow().len(), 3);
    }

    #[test]
    fn task_waker() {
        let base_waker = Arc::new(MockWaker::default());
        let mut n = 0;
        let ex = Executor::new();
        ex.register_base_waker(&base_waker.clone().into());
        ex.spawn(poll_fn(|cx| {
            n += 1;
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }));

        // Poll the spawned tasks, which should wake up right away
        ex.poll_spawned();
        assert_eq!(unsafe { (*ex.wake_queue.local.get()).len() }, 1);
        assert!(base_waker.get());
        // Poll the awoken task, which should wake up again
        ex.poll_tasks();
        assert_eq!(unsafe { (*ex.wake_queue.local.get()).len() }, 1);

        drop(ex);
        // Should have polled twice
        assert_eq!(n, 2);
    }

    #[test]
    fn cancel() {
        let ex = Executor::new();
        assert_eq!(ex.tasks.borrow().len(), 0);

        let task = ex.spawn(pending::<()>());
        // Cancel task while it's in the spawned list
        task.cancel();
        assert!(task.is_cancelled());
        ex.poll_tasks();
        ex.poll_spawned();
        assert_eq!(ex.tasks.borrow().len(), 0);

        let task = ex.spawn(pending::<()>());
        assert!(!task.is_cancelled());
        ex.poll_tasks();
        ex.poll_spawned();
        assert_eq!(ex.tasks.borrow().len(), 1);

        // Cancel task while it's in the task list
        task.cancel();
        ex.poll_tasks();
        ex.poll_spawned();
        assert_eq!(ex.tasks.borrow().len(), 0);
    }

    #[test]
    fn wake_queue() {
        let queue = WakeQueue::with_capacity(4);
        queue.push(12);
        queue.push(13);

        thread::scope(|s| {
            let queue = &queue;
            for i in 0..10 {
                s.spawn(move || queue.push(i));
            }
        });

        assert_eq!(queue.concurrent.len(), 10);
        assert_eq!(unsafe { (*queue.local.get()).len() }, 2);

        let mut elems = vec![];
        queue.drain_for_each(|e| elems.push(e));
        elems.sort_unstable();
        assert_eq!(elems, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13]);

        queue.push(12);
        queue.push(13);
        queue.reset(6);
        assert_eq!(queue.concurrent.len(), 0);
        assert_eq!(unsafe { (*queue.local.get()).len() }, 1);
        queue.drain_for_each(|e| assert_eq!(e, 6));
    }

    #[test]
    fn switch_waker_ex() {
        let _ = env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let ex = Executor::new();
        let waker1 = Arc::new(MockWaker::default());
        let waker2 = Arc::new(MockWaker::default());

        let mut fut = pin!(ex.run(async {
            let _bg = ex.spawn(sleep(Duration::from_millis(100)));
            sleep(Duration::from_millis(50)).await;
            // Have the future wait forever without polling the background task
            pending::<()>().await;
        }));

        // Poll future with waker1
        assert!(fut
            .as_mut()
            .poll(&mut Context::from_waker(&waker1.clone().into()))
            .is_pending());
        let now = Instant::now();
        // Wait until the 50ms sleep is done then invoke the reactor, which should notify waker1
        thread::sleep(Duration::from_millis(50));
        log::info!("{} ms to wakeup", now.elapsed().as_millis());
        REACTOR.with(|r| r.wait()).unwrap();
        assert!(waker1.get());

        // Poll future with waker2
        assert!(fut
            .as_mut()
            .poll(&mut Context::from_waker(&waker2.clone().into()))
            .is_pending());
        // Wait until the 100ms sleep is done then invoke the reactor, which should notify waker2
        // even though the sleep task is never polled after switching to waker2
        thread::sleep(Duration::from_millis(50));
        REACTOR.with(|r| r.wait()).unwrap();
        assert!(waker2.get());
    }

    #[test]
    fn switch_waker_join() {
        let waker1 = Arc::new(MockWaker::default());
        let waker2 = Arc::new(MockWaker::default());

        let mut fut = pin!(join!(
            sleep(Duration::from_millis(50)),
            sleep(Duration::from_millis(100))
        ));

        // Poll future with waker1
        assert!(fut
            .as_mut()
            .poll(&mut Context::from_waker(&waker1.clone().into()))
            .is_pending());
        // Wait until the 50ms sleep is done then invoke the reactor, which should notify waker1
        thread::sleep(Duration::from_millis(50));
        REACTOR.with(|r| r.wait()).unwrap();
        assert!(waker1.get());

        // Poll future with waker2
        assert!(fut
            .as_mut()
            .poll(&mut Context::from_waker(&waker2.clone().into()))
            .is_pending());
        // Wait until the 100ms sleep is done then invoke the reactor, which should notify waker2
        // even though the sleep task is never polled after switching to waker2
        thread::sleep(Duration::from_millis(50));
        REACTOR.with(|r| r.wait()).unwrap();
        assert!(waker2.get());
    }
}
