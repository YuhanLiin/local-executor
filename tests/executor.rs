use std::{
    cell::Cell,
    future::pending,
    net::{TcpListener, TcpStream},
    rc::Rc,
    time::{Duration, Instant},
};

use futures_lite::{AsyncReadExt, AsyncWriteExt, StreamExt};
use local_runtime::{
    io::Async,
    time::{sleep, timeout, Periodic},
    Executor,
};

#[test]
fn spawn_one() {
    let n = 10;
    let ex = Executor::new();
    let out = ex.run(async {
        let handle = ex.spawn(async { &n });
        handle.await
    });
    assert_eq!(*out, 10);
}

#[test]
fn spawn_parallel() {
    let start = Instant::now();
    let ex = Executor::new();
    ex.run(async {
        let task1 = ex.spawn(sleep(Duration::from_millis(100)));
        let task2 = ex.spawn(async {
            sleep(Duration::from_millis(50)).await;
            sleep(Duration::from_millis(70)).await;
        });
        sleep(Duration::from_millis(100)).await;
        task1.await;
        task2.await;
    });
    let elapsed = start.elapsed();
    assert!(elapsed > Duration::from_millis(120));
    assert!(elapsed < Duration::from_millis(150));
}

#[test]
fn spawn_recursive() {
    let n = 10;
    let nref = &n;
    let start = Instant::now();
    let ex = Rc::new(Executor::new());
    ex.run(async {
        #[allow(clippy::async_yields_async)]
        let task = ex.clone().spawn_rc(|ex| async move {
            sleep(Duration::from_millis(50)).await;
            ex.spawn(async move {
                sleep(Duration::from_millis(20)).await;
                nref
            })
        });

        ex.spawn(async move {
            let inner_task = task.await;
            assert_eq!(*inner_task.await, 10);
        })
        .await;
    });
    assert!(start.elapsed() > Duration::from_millis(70));
    assert_eq!(Rc::strong_count(&ex), 1);
}

#[test]
fn spawn_dropped() {
    let ex = Executor::new();
    ex.run(async {
        // Even though this task will never return, it doesn't matter because we don't await on it
        ex.spawn(pending::<()>());
    });
}

#[test]
fn cancelled() {
    let ex = Executor::new();
    ex.run(async {
        let task1 = ex.spawn(async { 3 });
        task1.cancel();
        assert!(timeout(task1, Duration::from_millis(10)).await.is_err());

        let task2 = ex.spawn(sleep(Duration::from_millis(5)));
        task2.cancel();
        assert!(timeout(task2, Duration::from_millis(10)).await.is_err());
    })
}

async fn periodic_test<'a>(n: &'a Cell<i32>, ex: Rc<Executor<'a>>) {
    let _bg = ex.clone().spawn_rc(move |ex| async move {
        let mut periodic = Periodic::periodic(Duration::from_millis(10));
        // Should run 4 times
        loop {
            periodic.next().await;
            println!("BG task {}", n.get());
            ex.spawn(async move { n.set(n.get() + 1) });
        }
    });

    let mut periodic = Periodic::periodic(Duration::from_millis(15));
    for _ in 0..3 {
        periodic.next().await;
        println!("Main task {}", n.get());
        ex.spawn(async move { n.set(n.get() + 1) });
    }
    // Final delay to ensure that all spawned tasks are polled
    sleep(Duration::from_micros(100)).await;
}

#[test]
fn spawn_periodic() {
    let n = Cell::new(0);
    let ex = Rc::new(Executor::new());
    ex.run(periodic_test(&n, ex.clone()));

    assert_eq!(n.get(), 7);
    assert_eq!(Rc::strong_count(&ex), 1);
}

#[test]
fn sub_executor_periodic() {
    let mut flag = false;

    let ex = Executor::new();
    ex.run(async {
        ex.spawn(async {
            sleep(Duration::from_millis(30)).await;
            flag = true;
        });

        let n = Cell::new(0);
        let sub = Rc::new(Executor::new());
        sub.run_async(periodic_test(&n, sub.clone())).await;
        assert_eq!(n.get(), 7);
        assert_eq!(Rc::strong_count(&sub), 1);
    });
    drop(ex);

    // run_async shouldn't block, so spawned tasks should run as well
    assert!(flag);
}

#[test]
fn sub_executor_one() {
    let ex = Executor::new();
    ex.run(async {
        let n = 10;
        let sub = Executor::new();
        let out = sub
            .run_async(async {
                let handle = sub.spawn(async {
                    sleep(Duration::from_millis(30)).await;
                    &n
                });
                handle.await
            })
            .await;
        assert_eq!(*out, n);
    });
}

#[test]
fn client_server() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Trace)
        .try_init();

    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr = listener.get_ref().local_addr().unwrap();

    let client = std::thread::spawn(move || {
        let ex = Executor::new();
        ex.run(async {
            for i in 1..=10 {
                let mut buf = [0u8; 11];
                let mut stream = Async::<TcpStream>::connect(addr).await.unwrap();
                stream.write_all(&[i; 5]).await.unwrap();
                stream.read(&mut buf).await.unwrap();
                assert_eq!(&buf, b"hello world");
            }
        })
    });

    let ex = Executor::new();
    ex.run(async {
        let mut incoming = listener.incoming();
        let mut tasks = vec![];
        for i in 1..=10 {
            let mut stream = incoming.next().await.unwrap().unwrap();
            log::info!("Server: connection {i}");
            let task = ex.spawn(async move {
                let mut buf = [0u8; 5];
                stream.read_exact(&mut buf).await.unwrap();
                stream.write_all(b"hello world").await.unwrap();
                assert!(buf.iter().all(|&b| b == i));
            });
            tasks.push(task);
        }

        for task in tasks {
            task.await;
        }
    });

    client.join().unwrap();
}
