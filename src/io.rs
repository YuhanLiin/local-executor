#[cfg(unix)]
use std::os::{
    fd::{AsFd, BorrowedFd},
    unix::net::UnixStream,
};
use std::{
    fs::File,
    future::poll_fn,
    io::{
        self, BufRead, BufReader, BufWriter, ErrorKind, LineWriter, Read, Stderr, StderrLock,
        Stdin, StdinLock, Stdout, StdoutLock, Write,
    },
    marker::PhantomData,
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    pin::Pin,
    process::{ChildStderr, ChildStdin, ChildStdout},
    task::{Context, Poll},
};

use futures_core::Stream;
use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

use crate::{
    reactor::{EventHandle, Interest},
    Reactor, REACTOR,
};

pub unsafe trait IoSafe {}

unsafe impl IoSafe for File {}
unsafe impl IoSafe for Stderr {}
unsafe impl IoSafe for Stdin {}
unsafe impl IoSafe for Stdout {}
unsafe impl IoSafe for StderrLock<'_> {}
unsafe impl IoSafe for StdinLock<'_> {}
unsafe impl IoSafe for StdoutLock<'_> {}
unsafe impl IoSafe for TcpStream {}
unsafe impl IoSafe for UdpSocket {}
#[cfg(unix)]
unsafe impl IoSafe for UnixStream {}
unsafe impl IoSafe for ChildStdin {}
unsafe impl IoSafe for ChildStderr {}
unsafe impl IoSafe for ChildStdout {}
unsafe impl<T: IoSafe> IoSafe for BufReader<T> {}
unsafe impl<T: IoSafe + Write> IoSafe for BufWriter<T> {}
unsafe impl<T: IoSafe + Write> IoSafe for LineWriter<T> {}
unsafe impl<T: IoSafe + ?Sized> IoSafe for &mut T {}
unsafe impl<T: IoSafe + ?Sized> IoSafe for Box<T> {}
unsafe impl<T: ?Sized> IoSafe for &T {}

// Deregisters the event handle on drop to ensure I/O safety
struct GuardedHandle(EventHandle);
impl Drop for GuardedHandle {
    fn drop(&mut self) {
        REACTOR.with(|r| r.deregister(&self.0));
    }
}

pub struct Async<T> {
    // Make sure the handle is dropped before the inner I/O type
    handle: GuardedHandle,
    inner: T,
    // Make this both !Send and !Sync
    _phantom: PhantomData<*const ()>,
}

impl<T> Unpin for Async<T> {}

#[cfg(unix)]
impl<T: AsFd> Async<T> {
    pub fn without_nonblocking(inner: T) -> Self {
        // SAFETY: GuardedHandle's Drop impl will deregister the FD
        let handle = GuardedHandle(unsafe { REACTOR.with(|r| r.register(&inner)) });
        Self {
            inner,
            handle,
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
    let previous = rustix::fs::fcntl_getfl(fd)?;
    let new = previous | rustix::fs::OFlags::NONBLOCK;
    if new != previous {
        rustix::fs::fcntl_setfl(fd, new)?;
    }
    Ok(())
}

impl<T> Async<T> {
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Deregisters the I/O handle from the reactor and return it
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn poll_event<'a, P, F>(
        &'a self,
        interest: Interest,
        cx: &mut Context<'_>,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a T) -> io::Result<P>,
    {
        match f(&self.inner) {
            Ok(n) => return Poll::Ready(Ok(n)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => return Poll::Ready(Err(err)),
        }
        REACTOR.with(|r| r.enable_event(&self.handle.0, interest, cx.waker()));
        Poll::Pending
    }

    fn poll_event_mut<'a, P, F>(
        &'a mut self,
        interest: Interest,
        cx: &mut Context<'_>,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        match f(&mut self.inner) {
            Ok(n) => return Poll::Ready(Ok(n)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => return Poll::Ready(Err(err)),
        }
        REACTOR.with(|r| r.enable_event(&self.handle.0, interest, cx.waker()));
        Poll::Pending
    }
}

impl<T: Read + IoSafe> AsyncRead for Async<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event_mut(Interest::Read, cx, |inner| inner.read(buf))
    }
}

impl<'a, T> AsyncRead for &'a Async<T>
where
    &'a T: Read,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event(Interest::Read, cx, |mut inner| inner.read(buf))
    }
}

impl<T: Write + IoSafe> AsyncWrite for Async<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event_mut(Interest::Write, cx, |inner| inner.write(buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_event_mut(Interest::Write, cx, |inner| inner.flush())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<'a, T> AsyncWrite for &'a Async<T>
where
    &'a T: Write,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_event(Interest::Write, cx, |mut inner| inner.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_event(Interest::Write, cx, |mut inner| inner.flush())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<T: BufRead + IoSafe> AsyncBufRead for Async<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        this.poll_event_mut(Interest::Read, cx, |inner| inner.fill_buf())
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        BufRead::consume(&mut self.inner, amt);
    }
}

impl Async<TcpListener> {
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        Async::new(TcpListener::bind(addr.into())?)
    }

    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Async<TcpStream>, SocketAddr)>> {
        self.poll_event(Interest::Read, cx, |inner| {
            inner
                .accept()
                .and_then(|(st, addr)| Async::new(st).map(|st| (st, addr)))
        })
    }

    pub async fn accept(&self) -> io::Result<(Async<TcpStream>, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub fn incoming(&self) -> IncomingTcp {
        IncomingTcp { listener: self }
    }
}

pub struct IncomingTcp<'a> {
    listener: &'a Async<TcpListener>,
}

impl Stream for IncomingTcp<'_> {
    type Item = io::Result<(Async<TcpStream>, SocketAddr)>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.listener.poll_accept(cx).map(Some)
    }
}

impl Async<TcpStream> {
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let addr = addr.into();
        let stream = Async::without_nonblocking(tcp_socket(&addr)?);
        poll_fn(|cx| stream.poll_event(Interest::Write, cx, |inner| connect(inner, &addr))).await?;
        Ok(stream)
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.poll_event(Interest::Read, cx, |inner| inner.peek(buf))).await
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
fn connect(tcp: &TcpStream, addr: &SocketAddr) -> io::Result<()> {
    match rustix::net::connect(tcp.as_fd(), addr) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::INPROGRESS) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, io::stderr, pin::pin, sync::Arc};

    use crate::{block_on, test::MockWaker};

    use super::*;

    #[test]
    fn deregister_on_drop() {
        let io = Async::without_nonblocking(stderr());
        assert!(!REACTOR.with(|r| r.is_empty()));
        drop(io);
        assert!(REACTOR.with(|r| r.is_empty()));
    }

    #[test]
    fn deregister_into_inner() {
        let io = Async::without_nonblocking(stderr());
        assert!(!REACTOR.with(|r| r.is_empty()));
        let _inner = io.into_inner();
        assert!(REACTOR.with(|r| r.is_empty()));
    }

    #[test]
    fn tcp() {
        let accept_waker = Arc::new(MockWaker::default());
        let connect_waker = Arc::new(MockWaker::default());

        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0)).unwrap();
        let addr = listener.get_ref().local_addr().unwrap();
        let mut accept = pin!(listener.accept());
        assert!(accept
            .as_mut()
            .poll(&mut Context::from_waker(&accept_waker.clone().into()))
            .is_pending());
        let mut connect = pin!(Async::<TcpStream>::connect(addr));
        assert!(connect
            .as_mut()
            .poll(&mut Context::from_waker(&accept_waker.clone().into()))
            .is_pending());

        block_on(async {
            let _accepted = accept.await.unwrap();
            let _conneted = connect.await.unwrap();
        });

        let mut connect = pin!(Async::<TcpStream>::connect(addr));
        assert!(connect
            .as_mut()
            .poll(&mut Context::from_waker(&connect_waker.into()))
            .is_pending());
    }
}
