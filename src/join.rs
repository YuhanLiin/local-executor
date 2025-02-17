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

impl<'a, T> Inflight<'a, T> {
    fn unwrap_done(self) -> T {
        match self {
            Inflight::Fut(_) => panic!("expected inflight future to be done"),
            Inflight::Done(val) => val,
        }
    }
}

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

fn poll_join<T>(
    inflights: &mut [Inflight<T>],
    wakers: &mut [Option<(Arc<JoinWaker>, Waker)>],
    cx: &mut Context,
) -> Poll<()> {
    let mut out = Poll::Ready(());
    for (inflight, waker) in inflights.iter_mut().zip(wakers.iter_mut()) {
        if let Inflight::Fut(fut) = inflight {
            let (waker_data, waker) = waker.get_or_insert_with(|| {
                let waker_data = Arc::new(JoinWaker::from(cx.waker().clone()));
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

impl<T: Unpin, const N: usize> Future for JoinFuture<'_, T, N> {
    type Output = [T; N];

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        poll_join(this.inflight.as_mut().unwrap(), &mut this.wakers, cx)
            .map(|_| this.inflight.take().unwrap().map(Inflight::unwrap_done))
    }
}

#[macro_export]
macro_rules! join {
    ($($fut:expr),+ $(,)?) => {
        async { $crate::JoinFuture::new([$(std::pin::pin!($fut)),+]).await }
    };
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    use crate::{
        block_on,
        timer::{sleep, timeout},
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
}
