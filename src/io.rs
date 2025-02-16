#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd};
use std::{
    future::poll_fn,
    io::{self, BufRead, ErrorKind, Read, Write},
    marker::PhantomData,
    net::{SocketAddr, TcpListener, TcpStream},
    pin::Pin,
    task::{Context, Poll},
};

use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

use crate::{
    reactor::{Interest, Source, WakeMode},
    Id, Reactor, REACTOR,
};

const READ_ID: usize = 0;
const WRITE_ID: usize = 1;

struct Registration {
    source: Source,
    ids: [Option<Id>; 2],
}

pub struct Async<T> {
    inner: T,
    reg: Registration,
    _phantom: PhantomData<*const ()>,
}

impl<T> Unpin for Async<T> {}

#[cfg(unix)]
impl<T: AsFd> Async<T> {
    pub fn without_nonblocking(inner: T) -> Self {
        let source = inner.as_fd().into();
        Self {
            inner,
            reg: Registration {
                source,
                ids: [None, None],
            },
            _phantom: PhantomData,
        }
    }

    pub fn new(inner: T) -> io::Result<Self> {
        set_nonblocking(inner.as_fd())?;
        Ok(Self::without_nonblocking(inner))
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    {
        let previous = rustix::fs::fcntl_getfl(fd)?;
        let new = previous | rustix::fs::OFlags::NONBLOCK;
        if new != previous {
            rustix::fs::fcntl_setfl(fd, new)?;
        }
    }
    Ok(())
}

impl Registration {
    fn disable_event(&mut self, event_idx: usize) {
        if let Some(id) = self.ids[event_idx] {
            REACTOR.with(|r| r.modify(id, &self.source, WakeMode::Disable))
        }
    }

    fn register_event(&mut self, event_idx: usize, interest: Interest, cx: &mut Context<'_>) {
        REACTOR.with(|r| match self.ids[event_idx] {
            Some(id) => r.modify(id, &self.source, WakeMode::Enable(cx.waker())),
            // Both read and write IDs are guaranteed to be dropped when Async is dropped, so they
            // won't outlive the inner file object
            None => unsafe {
                self.ids[event_idx] = Some(r.register(&self.source, interest, cx.waker().clone()))
            },
        });
    }
}

impl<T> Async<T> {
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    fn poll_event<'a, P, F>(
        &'a mut self,
        event_idx: usize,
        interest: Interest,
        cx: &mut Context<'_>,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        let res = match f(&mut self.inner) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        };
        if res.is_ready() {
            self.reg.disable_event(event_idx);
        } else {
            self.reg.register_event(event_idx, interest, cx);
        }
        res
    }

    fn poll_read_event<'a, P, F>(&'a mut self, cx: &mut Context<'_>, f: F) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        self.poll_event(READ_ID, Interest::Read, cx, f)
    }

    fn poll_write_event<'a, P, F>(&'a mut self, cx: &mut Context<'_>, f: F) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        self.poll_event(WRITE_ID, Interest::Write, cx, f)
    }
}

impl<T> Drop for Async<T> {
    fn drop(&mut self) {
        REACTOR.with(|r| {
            if let Some(id) = self.reg.ids[READ_ID] {
                r.deregister(id, &self.reg.source);
            }
            if let Some(id) = self.reg.ids[WRITE_ID] {
                r.deregister(id, &self.reg.source);
            }
        });
    }
}

impl<T: Read> AsyncRead for Async<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_read_event(cx, |inner| inner.read(buf))
    }
}

impl<T: Write> AsyncWrite for Async<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_event(cx, |inner| inner.write(buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_write_event(cx, |inner| inner.flush())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<T: BufRead> AsyncBufRead for Async<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        this.poll_read_event(cx, |inner| inner.fill_buf())
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        BufRead::consume(&mut self.inner, amt);
    }
}

impl Async<TcpListener> {
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        Async::new(TcpListener::bind(addr.into())?)
    }

    pub async fn accept(&mut self) -> io::Result<(Async<TcpStream>, SocketAddr)> {
        poll_fn(|cx| self.poll_read_event(cx, |inner| inner.accept()))
            .await
            .and_then(|(st, addr)| Async::new(st).map(|st| (st, addr)))
    }
}

impl Async<TcpStream> {
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let addr = addr.into();
        let mut stream = Async::without_nonblocking(tcp_socket(&addr)?);
        poll_fn(|cx| stream.poll_write_event(cx, |inner| connect(inner, &addr))).await?;
        Ok(stream)
    }

    pub async fn peek(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_read_event(cx, |inner| inner.peek(buf))).await
    }
}

fn tcp_socket(addr: &SocketAddr) -> io::Result<TcpStream> {
    use rustix::net::*;

    let af = match addr {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    };
    let type_ = SocketType::STREAM;
    let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
    let socket = socket_with(af, type_, flags, None)?;

    Ok(socket.into())
}

fn connect(tcp: &TcpStream, addr: &SocketAddr) -> io::Result<()> {
    rustix::net::connect(tcp.as_fd(), addr)?;
    Ok(())
}
