use std::net::{TcpListener, TcpStream};

use futures_lite::{future::zip, AsyncReadExt, AsyncWriteExt};
use simple_executor::{block_on, io::Async};

#[test]
fn single_thread_echo() {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Trace)
        .try_init();

    block_on(async {
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let addr = listener.get_ref().local_addr().unwrap();

        let fut1 = async {
            for i in 1..=10 {
                let mut buf = [0u8; 1000];
                let (mut stream, _) = listener.accept().await.unwrap();
                log::info!("Task1: Accept");
                stream.read_exact(&mut buf).await.unwrap();
                log::info!("Task1: Read");
                stream.write_all(b"hello world").await.unwrap();
                log::info!("Task1: Write");
                assert!(buf.iter().all(|&b| b == i));
            }
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
        };

        zip(fut1, fut2).await;
    });
}
