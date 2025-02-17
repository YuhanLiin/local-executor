use std::{
    future::{pending, ready},
    time::{Duration, Instant},
};

use simple_executor::{
    block_on,
    timer::{timeout, Timer},
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
    block_on(async {
        assert!(timeout(pending::<()>(), Duration::from_nanos(100))
            .await
            .is_err());
        assert_eq!(
            timeout(ready(12), Duration::from_nanos(100)).await.unwrap(),
            12
        );
    });
}
