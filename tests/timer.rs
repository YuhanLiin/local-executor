use std::{
    future::{pending, ready},
    time::{Duration, Instant},
};

use futures_lite::StreamExt;
use simple_executor::{
    block_on,
    timer::{timeout, Periodic, Timer},
};

#[test]
fn timer_test() {
    let now = Instant::now();
    block_on(async {
        Timer::delay(Duration::from_micros(800)).await;
    });
    assert!(now.elapsed() >= Duration::from_micros(800));
}

#[test]
fn timeout_test() {
    let now = Instant::now();
    block_on(async {
        assert!(timeout(pending::<()>(), Duration::from_nanos(100))
            .await
            .is_err());
        assert_eq!(
            timeout(ready(12), Duration::from_nanos(100)).await.unwrap(),
            12
        );
    });
    assert!(now.elapsed() >= Duration::from_nanos(100));
}

#[test]
fn periodic_test() {
    let mut count = 0;
    block_on(async {
        let fut = async {
            let mut periodic = Periodic::interval(Duration::from_micros(100));
            loop {
                periodic.next().await.unwrap();
                count += 1;
            }
        };
        timeout(fut, Duration::from_micros(750)).await.unwrap_err();
    });
    assert_eq!(count, 7);
}
