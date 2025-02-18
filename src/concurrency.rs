use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

use futures_core::Stream;

struct FlagWaker {
    waker: Waker,
    awoken: AtomicBool,
}

impl Wake for FlagWaker {
    fn wake(self: Arc<Self>) {
        self.set_awoken();
        self.waker.wake_by_ref();
    }
}

impl From<Waker> for FlagWaker {
    fn from(waker: Waker) -> Self {
        Self {
            waker,
            awoken: AtomicBool::new(true),
        }
    }
}

impl FlagWaker {
    fn check_awoken(&self) -> bool {
        self.awoken.swap(false, Ordering::Relaxed)
    }

    fn set_awoken(&self) {
        self.awoken.store(true, Ordering::Relaxed);
    }
}

type PinFut<'a, T> = Pin<&'a mut dyn Future<Output = T>>;
type PinStream<'a, T> = Pin<&'a mut dyn Stream<Item = T>>;

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
    wakers: [Option<(Arc<FlagWaker>, Waker)>; N],
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
        poll_join(this.inflight.as_mut().unwrap(), &mut this.wakers, cx)
            .map(|_| this.inflight.take().unwrap().map(Inflight::unwrap_done))
    }
}

fn poll_join<T>(
    inflights: &mut [Inflight<T>],
    wakers: &mut [Option<(Arc<FlagWaker>, Waker)>],
    cx: &mut Context,
) -> Poll<()> {
    let mut out = Poll::Ready(());
    for (inflight, waker) in inflights.iter_mut().zip(wakers.iter_mut()) {
        if let Inflight::Fut(fut) = inflight {
            let (waker_data, waker) = waker.get_or_insert_with(|| {
                let waker_data = Arc::new(FlagWaker::from(cx.waker().clone()));
                let waker = waker_data.clone().into();
                (waker_data, waker)
            });

            if waker_data.check_awoken() {
                if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(waker)) {
                    *inflight = Inflight::Done(out);
                    continue;
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
/// # Minimal polling
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

#[doc(hidden)]
pub struct MergeFutureStream<'a, T, const N: usize> {
    futures: [Option<PinFut<'a, T>>; N],
    wakers: [Option<(Arc<FlagWaker>, Waker)>; N],
    idx: usize,
    none_count: usize,
}

impl<'a, T, const N: usize> MergeFutureStream<'a, T, N> {
    pub fn new(futures: [PinFut<'a, T>; N]) -> Self {
        Self {
            futures: futures.map(Some),
            wakers: std::array::from_fn(|_| None),
            idx: 0,
            none_count: 0,
        }
    }
}

impl<T, const N: usize> Stream for MergeFutureStream<'_, T, N> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        poll_merged(
            &mut this.futures,
            &mut this.wakers,
            &mut this.idx,
            &mut this.none_count,
            cx,
            |fut, cx| fut.as_mut().poll(cx),
            |x| Some(x),
            |_| true,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_merged<P, O, T, PF, OF, NF>(
    pollers: &mut [Option<P>],
    wakers: &mut [Option<(Arc<FlagWaker>, Waker)>],
    idx: &mut usize,
    none_count: &mut usize,
    cx: &mut Context,
    mut poll_fn: PF,
    mut opt_fn: OF,
    mut none_fn: NF,
) -> Poll<Option<T>>
where
    PF: FnMut(&mut P, &mut Context) -> Poll<O>,
    OF: FnMut(O) -> Option<T>,
    NF: FnMut(&O) -> bool,
{
    let len = pollers.len();
    if *none_count == len {
        return Poll::Ready(None);
    }

    let (futs_past, futs_remain) = pollers.split_at_mut(*idx);
    let (wakers_past, wakers_remain) = wakers.split_at_mut(*idx);
    let iter_past = futs_past.iter_mut().zip(wakers_past.iter_mut());
    let iter_remain = futs_remain.iter_mut().zip(wakers_remain.iter_mut());
    // Prioritize the futures we haven't seen yet
    let iter = iter_remain.chain(iter_past);

    for (poller_opt, waker_pair) in iter {
        if let Some(poller) = poller_opt {
            let (waker_data, waker) = waker_pair.get_or_insert_with(|| {
                let waker_data = Arc::new(FlagWaker::from(cx.waker().clone()));
                let waker = waker_data.clone().into();
                (waker_data, waker)
            });

            if waker_data.check_awoken() {
                if let Poll::Ready(out) = poll_fn(poller, &mut Context::from_waker(waker)) {
                    if none_fn(&out) {
                        *poller_opt = None;
                        *none_count += 1;
                    }
                    if let Some(ret) = opt_fn(out) {
                        // Set the awoken flag so that the next time we poll, we'll start by
                        // polling the future/stream that just yielded a value
                        waker_data.set_awoken();
                        return Poll::Ready(Some(ret));
                    }
                }
            }
        }
        // Update index
        *idx = (*idx + 1) % len;
    }
    Poll::Pending
}

/// Poll the futures concurrently and return their outputs as a stream.
///
/// The stream yields `N` values, where `N` is the number of merged futures. The outputs will be
/// returned in the order in which the futures completed.
///
/// # Minimal polling
///
/// [`merge_futures`] will only poll each inner future when it is awoken, rather than polling all
/// inner futures on each iteration.
///
/// # Pinning
///
/// The input futures to this macro must be pinned to the local context via [`pin`](std::pin::pin).
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use std::pin::pin;
/// use futures_lite::StreamExt;
/// use local_runtime::time::sleep;
/// use local_runtime::merge_futures;
///
/// # local_runtime::block_on(async {
/// let a = pin!(async { 1 });
/// let b = pin!(async {
///     sleep(Duration::from_millis(5)).await;
///     2
/// });
/// let c = pin!(async {
///     sleep(Duration::from_millis(3)).await;
///     3
/// });
/// let mut stream = merge_futures!(a, b, c);
/// while let Some(x) = stream.next().await {
///     // Expect the values to be: 1, 3, 5
///     println!("Future returned: {x}");
/// }
/// # })
/// ```
#[macro_export]
macro_rules! merge_futures {
    ($($fut:expr),+ $(,)?) => {
        $crate::MergeFutureStream::new([$($fut),+])
    };
}

#[doc(hidden)]
pub struct MergeStream<'a, T, const N: usize> {
    streams: [Option<PinStream<'a, T>>; N],
    wakers: [Option<(Arc<FlagWaker>, Waker)>; N],
    idx: usize,
    none_count: usize,
}

impl<'a, T, const N: usize> MergeStream<'a, T, N> {
    pub fn new(streams: [PinStream<'a, T>; N]) -> Self {
        Self {
            streams: streams.map(Some),
            wakers: std::array::from_fn(|_| None),
            idx: 0,
            none_count: 0,
        }
    }
}

impl<T, const N: usize> Stream for MergeStream<'_, T, N> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        poll_merged(
            &mut this.streams,
            &mut this.wakers,
            &mut this.idx,
            &mut this.none_count,
            cx,
            |fut, cx| fut.as_mut().poll_next(cx),
            |o| o,
            |o| o.is_none(),
        )
    }
}

/// Run the streams concurrently and return their outputs one at a time.
///
/// [`merge_streams`] produces a stream that yields the outputs of the inner streams as they become
/// available, effectively interleaving the inner streams.
///
/// # Minimal polling
///
/// [`merge_streams`] will only poll each inner stream when it is awoken, rather than polling all
/// inner streams on each iteration.
///
/// # Pinning
///
/// The input streams to this macro must be pinned to the local context via [`pin`](std::pin::pin).
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use std::pin::pin;
/// use futures_lite::{Stream, StreamExt};
/// use local_runtime::time::Periodic;
/// use local_runtime::merge_streams;
///
/// # local_runtime::block_on(async {
/// let a = pin!(Periodic::periodic(Duration::from_millis(14)).map(|_| 1u8));
/// let b = pin!(Periodic::periodic(Duration::from_millis(6)).map(|_| 2u8));
/// let stream = merge_streams!(a, b);
/// assert_eq!(stream.take(6).collect::<Vec<_>>().await, &[2, 2, 1, 2, 2, 1]);
/// # })
/// ```
#[macro_export]
macro_rules! merge_streams {
    ($($fut:expr),+ $(,)?) => {
        $crate::MergeStream::new([$($fut),+])
    };
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        pin::pin,
        time::{Duration, Instant},
    };

    use futures_lite::StreamExt;

    use crate::{
        block_on,
        time::{sleep, timeout, Periodic},
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
    fn merge_futures() {
        block_on(async {
            let merged: Vec<_> = {
                let fut1 = pin!(async {
                    sleep(Duration::from_millis(1)).await;
                    12
                });
                let fut2 = pin!(async { 5 });
                merge_futures!(fut1, fut2).collect().await
            };
            assert_eq!(merged, [5, 12]);

            assert_eq!(
                merge_futures!(pin!(async { 1 }), pin!(async { 2 }), pin!(async { 3 }))
                    .collect::<Vec<_>>()
                    .await,
                &[1, 2, 3]
            );
        });
    }

    #[test]
    fn merge_timers() {
        let count = Cell::new(0);
        let data = block_on(async {
            merge_futures!(
                pin!(async {
                    let _ = timeout(CountFuture(&count), Duration::from_millis(10)).await;
                    1
                }),
                pin!(async {
                    let _ = timeout(CountFuture(&count), Duration::from_millis(20)).await;
                    2
                }),
                pin!(async {
                    let _ = timeout(CountFuture(&count), Duration::from_millis(5)).await;
                    3
                }),
                pin!(async {
                    let _ = timeout(CountFuture(&count), Duration::from_millis(25)).await;
                    4
                }),
                pin!(async {
                    let _ = timeout(CountFuture(&count), Duration::from_millis(30)).await;
                    5
                }),
            )
            .collect::<Vec<_>>()
            .await
        });
        assert_eq!(data, &[3, 1, 2, 4, 5]);
        // There should only be 10 polls, 2 for each future
        assert_eq!(count.get(), 10);
    }

    #[test]
    fn merge_same_time() {
        block_on(async {
            let a = pin!(async {
                sleep(Duration::from_millis(20)).await;
            });
            let b = pin!(async {
                sleep(Duration::from_millis(20)).await;
            });
            assert_eq!(merge_futures!(a, b).collect::<Vec<_>>().await.len(), 2);
        });
    }

    #[test]
    fn merge_periodic() {
        use futures_lite::StreamExt;
        use std::pin::pin;
        use std::time::Duration;

        block_on(async {
            let a = pin!(Periodic::periodic(Duration::from_millis(14)).map(|_| 1u8));
            let b = pin!(Periodic::periodic(Duration::from_millis(6)).map(|_| 2u8));
            let stream = merge_streams!(a, b);
            assert_eq!(
                stream.take(6).collect::<Vec<_>>().await,
                &[2, 2, 1, 2, 2, 1]
            );
        })
    }
}
