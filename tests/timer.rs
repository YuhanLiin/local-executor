use std::{
    future::{pending, ready},
    time::{Duration, Instant},
};

use futures_lite::StreamExt;
use local_runtime::{
    block_on,
    time::{timeout, Periodic, Timer},
};

#[test]
fn timer_test() {
    let now = Instant::now();
    block_on(async {
        Timer::delay(Duration::from_millis(10)).await;
    });
    assert!(now.elapsed() >= Duration::from_millis(10));
}

#[test]
fn timeout_test() {
    let now = Instant::now();
    block_on(async {
        assert!(timeout(pending::<()>(), Duration::from_millis(20))
            .await
            .is_err());
        assert_eq!(
            timeout(ready(12), Duration::from_millis(20)).await.unwrap(),
            12
        );
    });
    assert!(now.elapsed() >= Duration::from_millis(20));
}

#[test]
fn periodic_test() {
    let mut count = 0;
    block_on(async {
        let fut = async {
            let mut periodic = Periodic::periodic(Duration::from_millis(10));
            loop {
                periodic.next().await.unwrap();
                count += 1;
            }
        };
        timeout(fut, Duration::from_millis(75)).await.unwrap_err();
    });
    assert_eq!(count, 7);
}
