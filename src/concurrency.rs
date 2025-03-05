use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

use atomic_waker::AtomicWaker;
use futures_core::Stream;

struct FlagWaker {
    waker: Arc<AtomicWaker>,
    awoken: AtomicBool,
}

impl Wake for FlagWaker {
    fn wake(self: Arc<Self>) {
        self.set_awoken();
        self.waker.wake();
    }
}

impl FlagWaker {
    pub(crate) fn new(waker: Arc<AtomicWaker>) -> Self {
        Self {
            waker,
            // Initialize the flag to true so that the future gets polled the first time
            awoken: AtomicBool::new(true),
        }
    }

    pub(crate) fn waker_pair(waker: Arc<AtomicWaker>) -> (Arc<Self>, Waker) {
        let this = Arc::new(Self::new(waker));
        let waker = this.clone().into();
        (this, waker)
    }

    pub(crate) fn check_awoken(&self) -> bool {
        self.awoken.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn set_awoken(&self) {
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
#[must_use = "Futures do nothing unless polled"]
pub struct JoinFuture<'a, T, const N: usize> {
    base_waker: Arc<AtomicWaker>,
    inflight: Option<[Inflight<'a, T>; N]>,
    wakers: [(Arc<FlagWaker>, Waker); N],
}

impl<'a, T, const N: usize> JoinFuture<'a, T, N> {
    pub fn new(futures: [PinFut<'a, T>; N]) -> Self {
        let base_waker = Arc::new(AtomicWaker::new());
        Self {
            inflight: Some(futures.map(Inflight::Fut)),
            wakers: std::array::from_fn(|_| FlagWaker::waker_pair(base_waker.clone())),
            base_waker,
        }
    }
}

impl<T: Unpin, const N: usize> Future for JoinFuture<'_, T, N> {
    type Output = [T; N];

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.base_waker.register(cx.waker());
        poll_join(this.inflight.as_mut().unwrap(), &mut this.wakers)
            .map(|_| this.inflight.take().unwrap().map(Inflight::unwrap_done))
    }
}

fn poll_join<T>(inflights: &mut [Inflight<T>], wakers: &mut [(Arc<FlagWaker>, Waker)]) -> Poll<()> {
    let mut out = Poll::Ready(());
    for (inflight, (waker_data, waker)) in inflights.iter_mut().zip(wakers.iter_mut()) {
        if let Inflight::Fut(fut) = inflight {
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
/// This future will only poll each inner future when it is awoken, rather than polling all inner
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
#[must_use = "Streams do nothing unless polled"]
pub struct MergeFutureStream<'a, T, const N: usize> {
    base_waker: Arc<AtomicWaker>,
    futures: [Option<PinFut<'a, T>>; N],
    wakers: [(Arc<FlagWaker>, Waker); N],
    idx: usize,
    none_count: usize,
}

impl<'a, T, const N: usize> MergeFutureStream<'a, T, N> {
    pub fn new(futures: [PinFut<'a, T>; N]) -> Self {
        let base_waker = Arc::new(AtomicWaker::new());
        Self {
            futures: futures.map(Some),
            wakers: std::array::from_fn(|_| FlagWaker::waker_pair(base_waker.clone())),
            idx: 0,
            none_count: 0,
            base_waker,
        }
    }
}

impl<T, const N: usize> Stream for MergeFutureStream<'_, T, N> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.base_waker.register(cx.waker());
        poll_merged(
            &mut this.futures,
            &mut this.wakers,
            &mut this.idx,
            &mut this.none_count,
            |fut, cx| fut.as_mut().poll(cx),
            |x| Some(x),
            |_| true,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_merged<P, O, T, PF, OF, NF>(
    pollers: &mut [Option<P>],
    wakers: &mut [(Arc<FlagWaker>, Waker)],
    idx: &mut usize,
    none_count: &mut usize,
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

    let (futs_past, futs_remain) = pollers.split_at_mut(*idx);
    let (wakers_past, wakers_remain) = wakers.split_at_mut(*idx);
    let iter_past = futs_past.iter_mut().zip(wakers_past.iter_mut());
    let iter_remain = futs_remain.iter_mut().zip(wakers_remain.iter_mut());
    // Prioritize the futures we haven't seen yet
    let iter = iter_remain.chain(iter_past);

    for (poller_opt, (waker_data, waker)) in iter {
        if let Some(poller) = poller_opt {
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
        // If all the futures/streams have terminated, end the stream by returning none
        if *none_count == len {
            return Poll::Ready(None);
        }
    }
    Poll::Pending
}

/// Poll the futures concurrently and return their outputs as a stream.
///
/// Produces a stream that yields `N` values, where `N` is the number of merged futures. The
/// outputs will be returned in the order in which the futures completed.
///
/// # Minimal polling
///
/// This stream will only poll each inner future when it is awoken, rather than polling all
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
#[must_use = "Streams do nothing unless polled"]
pub struct MergeStream<'a, T, const N: usize> {
    base_waker: Arc<AtomicWaker>,
    streams: [Option<PinStream<'a, T>>; N],
    wakers: [(Arc<FlagWaker>, Waker); N],
    idx: usize,
    none_count: usize,
}

impl<'a, T, const N: usize> MergeStream<'a, T, N> {
    pub fn new(streams: [PinStream<'a, T>; N]) -> Self {
        let base_waker = Arc::new(AtomicWaker::new());
        Self {
            streams: streams.map(Some),
            wakers: std::array::from_fn(|_| FlagWaker::waker_pair(base_waker.clone())),
            idx: 0,
            none_count: 0,
            base_waker,
        }
    }
}

impl<T, const N: usize> Stream for MergeStream<'_, T, N> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.base_waker.register(cx.waker());
        poll_merged(
            &mut this.streams,
            &mut this.wakers,
            &mut this.idx,
            &mut this.none_count,
            |fut, cx| fut.as_mut().poll_next(cx),
            |o| o,
            |o| o.is_none(),
        )
    }
}

/// Run the streams concurrently and return their outputs one at a time.
///
/// Produces a stream that yields the outputs of the inner streams as they become available,
/// effectively interleaving the inner streams.
///
/// # Minimal polling
///
/// This stream will only poll each inner stream when it is awoken, rather than polling all inner
/// streams on each iteration.
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
/// let a = pin!(Periodic::periodic(Duration::from_millis(70)).map(|_| 1u8));
/// let b = pin!(Periodic::periodic(Duration::from_millis(30)).map(|_| 2u8));
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
    use crate::test::MockWaker;

    use super::*;

    #[test]
    fn flag_waker_multiple_wakers() {
        // Test that flag waker works even when the inner waker is swapped
        let wk1 = Arc::new(MockWaker::default());
        let wk2 = Arc::new(MockWaker::default());
        let atomic_waker = Arc::new(AtomicWaker::new());
        let (flag_waker_data, flag_waker) = FlagWaker::waker_pair(atomic_waker.clone());

        // The waker flag should be initialized as true
        assert!(flag_waker_data.check_awoken());
        assert!(!flag_waker_data.awoken.load(Ordering::Relaxed));
        atomic_waker.register(&wk1.clone().into());
        flag_waker.wake_by_ref();
        assert!(wk1.get());

        // After calling wake_by_ref(), the flag should be set to true
        assert!(flag_waker_data.check_awoken());
        assert!(!flag_waker_data.awoken.load(Ordering::Relaxed));
        atomic_waker.register(&wk2.clone().into());
        flag_waker.wake_by_ref();
        assert!(wk2.get());
    }
}
