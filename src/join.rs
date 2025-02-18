use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

struct JoinWaker {
    waker: Waker,
    awoken: AtomicBool,
}

impl Wake for JoinWaker {
    fn wake(self: Arc<Self>) {
        self.awoken.store(true, Ordering::Relaxed);
        self.waker.wake_by_ref();
    }
}

impl From<Waker> for JoinWaker {
    fn from(waker: Waker) -> Self {
        Self {
            waker,
            awoken: AtomicBool::new(true),
        }
    }
}

impl JoinWaker {
    fn check_awoken(&self) -> bool {
        self.awoken.swap(false, Ordering::Relaxed)
    }
}

type PinFut<'a, T> = Pin<&'a mut dyn Future<Output = T>>;

enum Inflight<'a, T> {
    Fut(PinFut<'a, T>),
    Done(T),
}

impl<T> Inflight<'_, T> {
    fn unwrap_done(self) -> T {
        match self {
            Inflight::Fut(_) => panic!("expected inflight future to be done"),
            Inflight::Done(val) => val,
        }
    }
}

#[doc(hidden)]
pub struct JoinFuture<'a, T, const N: usize> {
    inflight: Option<[Inflight<'a, T>; N]>,
    wakers: [Option<(Arc<JoinWaker>, Waker)>; N],
}

impl<'a, T, const N: usize> JoinFuture<'a, T, N> {
    pub fn new(futures: [PinFut<'a, T>; N]) -> Self {
        Self {
            inflight: Some(futures.map(Inflight::Fut)),
            wakers: std::array::from_fn(|_| None),
        }
    }
}

impl<T: Unpin, const N: usize> Future for JoinFuture<'_, T, N> {
    type Output = [T; N];

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        poll_join(
            this.inflight.as_mut().unwrap(),
            &mut this.wakers,
            cx,
            |_| false,
        )
        .map(|_| this.inflight.take().unwrap().map(Inflight::unwrap_done))
    }
}

#[doc(hidden)]
pub struct TryJoinFuture<'a, T, E, const N: usize>(JoinFuture<'a, Result<T, E>, N>);

impl<'a, T, E, const N: usize> TryJoinFuture<'a, T, E, N> {
    pub fn new(futures: [PinFut<'a, Result<T, E>>; N]) -> Self {
        Self(JoinFuture::new(futures))
    }
}

fn unwrap_err<T, E>(res: Result<T, E>) -> E {
    let Err(err) = res else {
        panic!("expected error")
    };
    err
}

impl<T, E, const N: usize> Future for TryJoinFuture<'_, T, E, N>
where
    Result<T, E>: Unpin,
{
    type Output = Result<[T; N], E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        poll_join(
            this.0.inflight.as_mut().unwrap(),
            &mut this.0.wakers,
            cx,
            |res| res.is_err(),
        )
        .map(|poll_res| {
            poll_res
                .map(|_| {
                    this.0.inflight.take().unwrap().map(|inflight| {
                        inflight
                            .unwrap_done()
                            .unwrap_or_else(|_| panic!("unexpected error"))
                    })
                })
                .map_err(unwrap_err)
        })
    }
}

fn poll_join<T, F: FnMut(&T) -> bool>(
    inflights: &mut [Inflight<T>],
    wakers: &mut [Option<(Arc<JoinWaker>, Waker)>],
    cx: &mut Context,
    mut stop_predicate: F,
) -> Poll<Result<(), T>> {
    let mut out = Poll::Ready(Ok(()));
    for (inflight, waker) in inflights.iter_mut().zip(wakers.iter_mut()) {
        if let Inflight::Fut(fut) = inflight {
            let (waker_data, waker) = waker.get_or_insert_with(|| {
                let waker_data = Arc::new(JoinWaker::from(cx.waker().clone()));
                let waker = waker_data.clone().into();
                (waker_data, waker)
            });

            if waker_data.check_awoken() {
                if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(waker)) {
                    if stop_predicate(&out) {
                        return Poll::Ready(Err(out));
                    } else {
                        *inflight = Inflight::Done(out);
                        continue;
                    }
                }
            }
            out = Poll::Pending;
        }
    }
    out
}

/// Poll multiple futures concurrently, returning a future that outputs an array of all results
/// once all futures have completed.
///
/// [`join`] will only poll each inner future when it is awoken, rather than polling all inner
/// futures on each iteration.
///
/// # Caveat
///
/// The futures must all have the same output type, which must be `Unpin`.
///
/// # Examples
///
/// ```
/// use local_runtime::join;
///
/// # local_runtime::block_on(async {
/// let a = async { 1 };
/// let b = async { 2 };
/// let c = async { 3 };
/// assert_eq!(join!(a, b, c).await, [1, 2, 3]);
/// # })
/// ```
#[macro_export]
macro_rules! join {
    ($($fut:expr),+ $(,)?) => {
        async { $crate::JoinFuture::new([$(std::pin::pin!($fut)),+]).await }
    };
}

/// Poll multiple futures concurrently, resolving to a [`Result`](std::result::Result) containing
/// either an array of all results or an error.
///
/// Unlike [`join`], [`try_join`] will return early if any of the futures returns an error,
/// dropping all the other futures.
///
/// # Caveat
///
/// The futures must all have the same output type, which must be `Unpin` and a
/// [`Result`](std::result::Result).
///
/// # Examples
///
/// ```
/// use local_runtime::try_join;
///
/// # local_runtime::block_on(async {
/// let a = async { Err::<u32, i32>(-1) };
/// let b = async { Ok::<u32, i32>(10) };
/// assert_eq!(try_join!(a, b).await, Err(-1));
/// # })
/// ```
#[macro_export]
macro_rules! try_join {
    ($($fut:expr),+ $(,)?) => {
        async { $crate::TryJoinFuture::new([$(std::pin::pin!($fut)),+]).await }
    };
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        future::pending,
        time::{Duration, Instant},
    };

    use crate::{
        block_on,
        time::{sleep, timeout},
    };

    use super::*;

    struct CountFuture<'a>(&'a Cell<u8>);

    impl Future for CountFuture<'_> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.set(self.0.get() + 1);
            Poll::Pending
        }
    }

    #[test]
    fn join() {
        let count = Cell::new(0);
        let now = Instant::now();
        let joined = async {
            join!(
                async {
                    let _ = timeout(CountFuture(&count), Duration::from_micros(500)).await;
                },
                async {
                    let _ = timeout(CountFuture(&count), Duration::from_micros(200)).await;
                },
                async {
                    let _ = timeout(CountFuture(&count), Duration::from_millis(10)).await;
                },
            )
            .await;
        };
        block_on(joined);
        assert!(now.elapsed() >= Duration::from_millis(10));
        // JoinFuture shouldn't be polling every future each time, so there should only be 6 polls
        assert_eq!(count.get(), 6);
    }

    #[test]
    fn scoping() {
        let out = block_on(async {
            let joined = {
                let fut1 = async { 5 };
                let fut2 = async {
                    sleep(Duration::from_nanos(1)).await;
                    12
                };
                join!(fut1, fut2)
            };
            joined.await
        });
        assert_eq!(out, [5, 12]);
    }

    #[test]
    fn try_join() {
        let err = block_on(try_join!(
            async { Err("error") },
            pending::<Result<(), &str>>()
        ))
        .unwrap_err();
        assert_eq!(err, "error");
    }
}
