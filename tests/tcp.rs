use std::net::{TcpListener, TcpStream};

use futures_lite::{AsyncReadExt, AsyncWriteExt, StreamExt};
use local_runtime::{block_on, io::Async, join};

#[test]
fn single_thread_echo() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Trace)
        .try_init();

    let mut done = (false, false);
    block_on(async {
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let addr = listener.get_ref().local_addr().unwrap();

        let fut1 = async {
            let mut incoming = listener.incoming();
            for i in 1..=10 {
                let mut buf = [0u8; 1000];
                let mut stream = incoming.next().await.unwrap().unwrap();
                log::info!("Task1: Accept");
                stream.read_exact(&mut buf).await.unwrap();
                log::info!("Task1: Read");
                stream.write_all(b"hello world").await.unwrap();
                log::info!("Task1: Write");
                assert!(buf.iter().all(|&b| b == i));
            }
            done.0 = true;
        };

        let fut2 = async {
            for i in 1..=10 {
                let mut buf = [0u8; 11];
                let mut stream = Async::<TcpStream>::connect(addr).await.unwrap();
                log::info!("Task2: Connect");
                for _ in 0..10 {
                    stream.write_all(&[i; 100]).await.unwrap();
                }
                log::info!("Task2: Write");
                stream.read(&mut buf).await.unwrap();
                log::info!("Task2: Read");
                assert_eq!(&buf, b"hello world");
            }
            done.1 = true;
        };

        join!(fut1, fut2).await;
        assert!(done.0);
        assert!(done.1);
    });
}

#[test]
fn echo_with_channel() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Trace)
        .try_init();

    let (send, recv) = flume::bounded(0);
    let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr = listener.get_ref().local_addr().unwrap();

    let th = std::thread::spawn(move || {
        block_on(async {
            let mut stream = Async::<TcpStream>::connect(addr).await.unwrap();
            send.send_async(b"deadbeef").await.unwrap();
            let mut data = vec![];
            stream.read_to_end(&mut data).await.unwrap();
            assert_eq!(data, b"deadbeef");
        });
    });

    block_on(async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let msg = recv.recv_async().await.unwrap();
        stream.write_all(msg).await.unwrap();
    });

    th.join().unwrap();
}
