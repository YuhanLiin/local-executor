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

pub struct JoinFuture<'a, const N: usize> {
    futures: [Option<Pin<&'a mut dyn Future<Output = ()>>>; N],
    wakers: [Option<Arc<JoinWaker>>; N],
}

impl<'a, const N: usize> JoinFuture<'a, N> {
    pub fn new(futures: [Pin<&'a mut dyn Future<Output = ()>>; N]) -> Self {
        Self {
            futures: futures.map(Some),
            wakers: std::array::from_fn(|_| None),
        }
    }
}

impl<const N: usize> Future for JoinFuture<'_, N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut out = Poll::Ready(());

        for (fut_opt, waker) in this.futures.iter_mut().zip(&mut this.wakers) {
            if let Some(fut) = fut_opt {
                let waker =
                    waker.get_or_insert_with(|| Arc::new(JoinWaker::from(cx.waker().clone())));

                if waker.check_awoken()
                    && fut
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker.clone().into()))
                        .is_ready()
                {
                    *fut_opt = None;
                } else {
                    out = Poll::Pending;
                }
            }
        }
        out
    }
}

#[macro_export]
macro_rules! join {
    ($($fut:expr),+ $(,)?) => {
        $crate::JoinFuture::new([$(std::pin::pin!($fut)),+])
    };
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    use crate::{block_on, timer::timeout};

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
        block_on(join!(
            async {
                let _ = timeout(CountFuture(&count), Duration::from_micros(500)).await;
            },
            async {
                let _ = timeout(CountFuture(&count), Duration::from_micros(200)).await;
            },
            async {
                let _ = timeout(CountFuture(&count), Duration::from_millis(10)).await;
            },
        ));
        assert!(now.elapsed() >= Duration::from_millis(10));
        // JoinFuture shouldn't be polling every future each time, so there should only be 6 polls
        assert_eq!(count.get(), 6);
    }
}
