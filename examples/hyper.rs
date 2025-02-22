use std::{
    error::Error,
    io::{stdout, Write},
    net::{TcpStream, ToSocketAddrs},
    pin::{pin, Pin},
    task::{Context, Poll},
};

use futures_io::{AsyncRead, AsyncWrite};
use futures_lite::StreamExt;
use http_body_util::BodyExt;
use hyper::{header, Request};
use local_runtime::{block_on, io::Async, merge_futures, Executor};
use pin_project_lite::pin_project;

pin_project! {
    struct HyperIo<T>{
        #[pin]
        inner: T
    }
}

impl<T> HyperIo<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: AsyncWrite> hyper::rt::Write for HyperIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_close(cx)
    }
}

impl<T: AsyncRead> hyper::rt::Read for HyperIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let remaining = unsafe { buf.as_mut() };
        for b in &mut *remaining {
            b.write(0);
        }
        let remaining = unsafe { std::mem::transmute::<_, &mut [u8]>(remaining) };
        match self.project().inner.poll_read(cx, remaining) {
            Poll::Ready(Ok(n)) => {
                unsafe { buf.advance(n) };
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    //let _ = env_logger::builder()
    //.is_test(true)
    //.filter_level(log::LevelFilter::Trace)
    //.try_init();

    let url = "http://httpbin.org/ip".parse::<hyper::Uri>()?;
    let host = url.host().unwrap();
    let port = url.port_u16().unwrap_or(80);
    let addr = format!("{}:{}", host, port)
        .to_socket_addrs()?
        .next()
        .unwrap();

    let ex = Executor::new();
    ex.run(async {
        let stream = HyperIo::new(Async::<TcpStream>::connect(addr).await?);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(stream).await?;

        // This task will never end, so don't await it
        ex.spawn(async {
            if let Err(err) = conn.await {
                eprintln!("Connection failed: {:?}", err);
            }
        });

        let authority = url.authority().unwrap().clone();
        let req = Request::builder()
            .uri(url)
            .header(header::HOST, authority.as_str())
            .body(String::new())?;

        let mut res = sender.send_request(req).await?;
        println!("Response status: {}", res.status());

        while let Some(next) = res.frame().await {
            let frame = next?;
            if let Some(chunk) = frame.data_ref() {
                // Use sync writes here because async writes don't really work on stdout
                stdout().write_all(chunk)?;
            }
        }
        Ok(())
    })
}
