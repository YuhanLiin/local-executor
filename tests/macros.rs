use std::{
    cell::Cell,
    future::Future,
    pin::{pin, Pin},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures_lite::{stream, StreamExt};

use local_runtime::{
    block_on, join, merge_futures, merge_streams,
    time::{sleep, timeout, Periodic},
};

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
                let _ = timeout(CountFuture(&count), Duration::from_millis(5)).await;
            },
            async {
                let _ = timeout(CountFuture(&count), Duration::from_millis(2)).await;
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

// Requires too much precision for poll-timeout based implementation
#[cfg(target_os = "linux")]
#[test]
fn merge_periodic() {
    block_on(async {
        let a = pin!(Periodic::periodic(Duration::from_millis(14)).map(|_| 1u8));
        let b = pin!(Periodic::periodic(Duration::from_millis(6)).map(|_| 2u8));
        let c = pin!(stream::unfold(0, |n| async move {
            if n < 2 {
                sleep(Duration::from_millis(5)).await;
                Some((3, n + 1))
            } else {
                None
            }
        }));
        let stream = merge_streams!(a, b, c);
        assert_eq!(
            stream.take(8).collect::<Vec<_>>().await,
            &[3, 2, 3, 2, 1, 2, 2, 1]
        );
    })
}

#[test]
fn single_entry() {
    block_on(async {
        assert_eq!(join!(async { 1 }).await[0], 1);
        assert_eq!(
            merge_futures!(pin!(async { 2 })).collect::<Vec<_>>().await,
            &[2]
        );
        println!("wtf");
        let stream = pin!(stream::iter([1, 2, 3].iter()));
        assert_eq!(
            merge_streams!(stream).collect::<Vec<i32>>().await,
            &[1, 2, 3]
        );
    });
}
