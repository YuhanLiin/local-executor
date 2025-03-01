use std::net::UdpSocket;

use local_runtime::{block_on, io::Async, join};

#[test]
fn unconnected() {
    let socket1 = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr1 = socket1.get_ref().local_addr().unwrap();
    let socket2 = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr2 = socket2.get_ref().local_addr().unwrap();

    block_on(async {
        let fut1 = async {
            let mut data = [0u8; 5];
            let (n, addr) = socket1.recv_from(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            assert_eq!(n, 5);
            assert_eq!(addr, addr2);
        };

        let fut2 = async {
            socket2.send_to(b"hello", addr1).await.unwrap();
        };

        join!(fut1, fut2).await;
    });
}

#[test]
fn connected() {
    let socket1 = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr1 = socket1.get_ref().local_addr().unwrap();
    let socket2 = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr2 = socket2.get_ref().local_addr().unwrap();

    block_on(async {
        let fut1 = async {
            socket1.connect(addr2).unwrap();
            let mut data = [0u8; 5];
            let n = socket1.recv(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            assert_eq!(n, 5);
        };

        let fut2 = async {
            socket2.connect(addr1).unwrap();
            socket2.send(b"hello").await.unwrap();
        };

        join!(fut1, fut2).await;
    });
}

#[test]
fn loopback() {
    let socket = Async::<UdpSocket>::bind(([127, 0, 0, 1], 0)).unwrap();
    let addr = socket.get_ref().local_addr().unwrap();
    socket.connect(addr).unwrap();

    block_on(async {
        let fut1 = async {
            let mut data = [0u8; 5];
            let n = socket.recv(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            assert_eq!(n, 5);
            let n = socket.recv(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            assert_eq!(n, 5);
        };

        let fut2 = async {
            socket.send(b"hello").await.unwrap();
            socket.send(b"hello").await.unwrap();
        };

        join!(fut1, fut2).await;
    });
}
