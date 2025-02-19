//! Async I/O primitives
//!
//! See [`Async`] for more details.

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

/// Types whose I/O trait implementations do not move or drop the underlying I/O object
///
/// The I/O object inside [`Async`] cannot be closed before the [`Async`] is dropped, because
/// `[Async]` deregisters the I/O object from the reactor on drop. Closing the I/O before
/// deregistering leads to the reactor holding a dangling I/O handle, which violates I/O safety.
///
/// As such, functions that grant mutable access to the inner I/O object are unsafe, because they
/// may move or drop the underlying I/O. Unfortunately, [`Async`] needs to call I/O traits such as
/// [`Read`] and [`Write`] to implement the async version of those traits.
///
/// To signal that those traits are safe to implement for an I/O type, it must implement
/// [`IoSafe`], which acts as a promise that the I/O type doesn't move or drop itself in its I/O
/// trait implementations.
///
/// This trait is implemented for `std` I/O types.
///
/// # Safety
///
/// Implementors of [`IoSafe`] must not drop or move its underlying I/O source in its
/// implementations of [`Read`], [`Write`], and [`BufRead`]. Specifically, the "underlying I/O
/// source" is defined as the I/O primitive corresponding to the type's `AsFd`/`AsSocket`
/// implementation.
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

/// [`IoSafe`] cannot be unconditionally implemented for references, because non-mutable references
/// can still drop or move internal fields via interior mutability.
unsafe impl<T: IoSafe + ?Sized> IoSafe for &T {}

// Deregisters the event handle on drop to ensure I/O safety
struct GuardedHandle(EventHandle);
impl Drop for GuardedHandle {
    fn drop(&mut self) {
        REACTOR.with(|r| r.deregister(&self.0));
    }
}

/// Async adapter for I/O types
///
/// This type puts the I/O object into non-blocking mode, registers it on the reactor, and provides
/// an async interface for it, including the [`AsyncRead`] and [`AsyncWrite`] traits.
///
/// # Supported types
///
/// [`Async`] supports any type that implements `AsFd` or `AsSocket`. This includes all standard
/// networking types. However, `Async` should not be used with types like [`File`] or [`Stdin`],
/// because they don't work well in non-blocking mode.
///
/// # Concurrency
///
/// Most operations on [`Async`] take `&self`, so tasks can access it concurrently. However, only
/// one task can read at a time, and only one task can write at a time. It is fine to have one task
/// reading while another one writes, but it is not fine to have multiple tasks reading or multiple
/// tasks writing. Doing so will lead to wakers being lost, which can prevent tasks from waking up
/// properly.
///
/// # Examples
///
/// ```no_run
/// use std::net::TcpStream;
/// use local_runtime::io::Async;
/// use futures_lite::AsyncWriteExt;
///
/// # local_runtime::block_on(async {
/// let mut stream = Async::<TcpStream>::connect(([127, 0, 0, 1], 8000)).await?;
/// stream.write_all(b"hello").await?;
/// # Ok::<_, std::io::Error>(())
/// # });
/// ```
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
    /// Create a new async adapter around the I/O object without setting it to non-blocking mode.
    ///
    /// This will register the I/O object onto the reactor.
    ///
    /// The caller must ensure the I/O object has already been set to non-blocking mode. Otherwise
    /// it may block the async runtime, preventing other futures from executing on the same thread.
    pub fn without_nonblocking(inner: T) -> Self {
        // SAFETY: GuardedHandle's Drop impl will deregister the FD
        let handle = GuardedHandle(unsafe { REACTOR.with(|r| r.register(&inner)) });
        Self {
            inner,
            handle,
            _phantom: PhantomData,
        }
    }

    /// Create a new async adapter around the I/O object.
    ///
    /// This will set the I/O object to non-blocking mode and register it onto the reactor.
    pub fn new(inner: T) -> io::Result<Self> {
        set_nonblocking(inner.as_fd())?;
        Ok(Self::without_nonblocking(inner))
    }
}

#[cfg(unix)]
pub(crate) fn set_nonblocking(fd: BorrowedFd) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    rustix::io::ioctl_fionbio(fd, true)?;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let previous = rustix::fs::fcntl_getfl(fd)?;
        let new = previous | rustix::fs::OFlags::NONBLOCK;
        if new != previous {
            rustix::fs::fcntl_setfl(fd, new)?;
        }
    }
    Ok(())
}

impl<T> Async<T> {
    /// Get reference to inner I/O handle
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Deregisters the I/O handle from the reactor and return it
    pub fn into_inner(self) -> T {
        self.inner
    }

    unsafe fn poll_event<'a, P, F>(
        &'a self,
        interest: Interest,
        cx: &mut Context,
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

    unsafe fn poll_event_mut<'a, P, F>(
        &'a mut self,
        interest: Interest,
        cx: &mut Context,
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

    /// Perform a single non-blocking read operation
    ///
    /// The underlying I/O object is read by the `f` closure once. If the result is
    /// [`io::ErrorKind::WouldBlock`], then this method returns [`Poll::Pending`] and tells the
    /// reactor to notify the context `cx` when the I/O object becomes readable.
    ///
    /// The closure should not perform multiple I/O operations, such as calling
    /// [`Write::write_all`]. This is because the closure is restarted for each poll, so the
    /// first I/O operation will be repeated and the subsequent operations won't be completed.
    ///
    /// # Safety
    ///
    /// The closure must not drop the underlying I/O object.
    ///
    /// # Example
    ///
    /// The non-blocking read operation can be converted into a future by wrapping this method in
    /// [`poll_fn`].
    ///
    /// ```no_run
    /// use std::net::TcpListener;
    /// use std::future::poll_fn;
    /// use local_runtime::io::Async;
    ///
    /// # local_runtime::block_on(async {
    /// let mut listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// // Accept connections asynchronously
    /// let (stream, addr) = poll_fn(|cx| unsafe { listener.poll_read_with(cx, |l| l.accept()) }).await?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub unsafe fn poll_read_with<'a, P, F>(&'a self, cx: &mut Context, f: F) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a T) -> io::Result<P>,
    {
        self.poll_event(Interest::Read, cx, f)
    }

    /// Same as [`Self::poll_read_with`], but takes a mutable reference in the closure
    ///
    /// # Safety
    ///
    /// The closure must not drop the underlying I/O object.
    pub unsafe fn poll_read_with_mut<'a, P, F>(
        &'a mut self,
        cx: &mut Context,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        self.poll_event_mut(Interest::Read, cx, f)
    }

    /// Perform a single non-blocking write operation
    ///
    /// The underlying I/O object is write by the `f` closure once. If the result is
    /// [`io::ErrorKind::WouldBlock`], then this method returns [`Poll::Pending`] and tells the
    /// reactor to notify the context `cx` when the I/O object becomes writable.
    ///
    /// The closure should not perform multiple I/O operations, such as calling
    /// [`Write::write_all`]. This is because the closure is restarted for each poll, so the
    /// first I/O operation will be repeated and the subsequent operations won't be completed.
    ///
    /// # Safety
    ///
    /// The closure must not drop the underlying I/O object.
    ///
    /// # Example
    ///
    /// The non-blocking write operation can be converted into a future by wrapping this method in
    /// [`poll_fn`].
    ///
    /// ```no_run
    /// use std::net::TcpStream;
    /// use std::future::poll_fn;
    /// use std::io::Write;
    /// use local_runtime::io::Async;
    ///
    /// # local_runtime::block_on(async {
    /// let mut stream = Async::<TcpStream>::connect(([127, 0, 0, 1], 8000)).await?;
    /// // Write some data asynchronously
    /// poll_fn(|cx| unsafe { stream.poll_write_with(cx, |mut s| s.write(b"hello")) }).await?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub unsafe fn poll_write_with<'a, P, F>(&'a self, cx: &mut Context, f: F) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a T) -> io::Result<P>,
    {
        self.poll_event(Interest::Write, cx, f)
    }

    /// Same as [`Self::poll_write_with`], but takes a mutable reference in the closure
    ///
    /// # Safety
    ///
    /// The closure must not drop the underlying I/O object.
    pub unsafe fn poll_write_with_mut<'a, P, F>(
        &'a mut self,
        cx: &mut Context,
        f: F,
    ) -> Poll<io::Result<P>>
    where
        F: FnOnce(&'a mut T) -> io::Result<P>,
    {
        self.poll_event_mut(Interest::Write, cx, f)
    }

    async fn wait_for_event_ready(&self, interest: Interest) {
        let mut first_call = true;
        poll_fn(|cx| {
            if first_call {
                first_call = false;
                // First enable the event
                REACTOR.with(|r| r.enable_event(&self.handle.0, interest, cx.waker()));
                Poll::Pending
            } else {
                // Then, check if the event is ready
                match REACTOR.with(|r| r.is_event_ready(&self.handle.0, interest)) {
                    true => Poll::Ready(()),
                    // If not, then update the event waker
                    false => {
                        REACTOR.with(|r| r.enable_event(&self.handle.0, interest, cx.waker()));
                        Poll::Pending
                    }
                }
            }
        })
        .await
    }

    /// Waits until the I/O object is available to write without blocking
    pub async fn writable(&self) {
        self.wait_for_event_ready(Interest::Write).await
    }

    /// Waits until the I/O object is available to read without blocking
    pub async fn readable(&self) {
        self.wait_for_event_ready(Interest::Read).await
    }
}

impl<T: Read + IoSafe> AsyncRead for Async<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Safety: IoSafe is implemented
        unsafe { self.poll_event_mut(Interest::Read, cx, |inner| inner.read(buf)) }
    }
}

impl<'a, T> AsyncRead for &'a Async<T>
where
    &'a T: Read + IoSafe,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Safety: IoSafe is implemented
        unsafe { self.poll_event(Interest::Read, cx, |mut inner| inner.read(buf)) }
    }
}

impl<T: Write + IoSafe> AsyncWrite for Async<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Safety: IoSafe is implemented
        unsafe { self.poll_event_mut(Interest::Write, cx, |inner| inner.write(buf)) }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: IoSafe is implemented
        unsafe { self.poll_event_mut(Interest::Write, cx, |inner| inner.flush()) }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<'a, T> AsyncWrite for &'a Async<T>
where
    &'a T: Write + IoSafe,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        // Safety: IoSafe is implemented
        unsafe { self.poll_event(Interest::Write, cx, |mut inner| inner.write(buf)) }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // Safety: IoSafe is implemented
        unsafe { self.poll_event(Interest::Write, cx, |mut inner| inner.flush()) }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl<T: BufRead + IoSafe> AsyncBufRead for Async<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        // Safety: IoSafe is implemented
        unsafe { this.poll_event_mut(Interest::Read, cx, |inner| inner.fill_buf()) }
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        BufRead::consume(&mut self.inner, amt);
    }
}

impl Async<TcpListener> {
    /// Create a TCP listener bound to a specific address
    ///
    /// # Example
    ///
    /// Bind the TCP listener to an OS-assigned port at 127.0.0.1.
    ///
    /// ```no_run
    /// use std::net::TcpListener;
    /// use local_runtime::io::Async;
    ///
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// # Ok::<_, std::io::Error>(())
    /// ```
    pub fn bind<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        Async::new(TcpListener::bind(addr.into())?)
    }

    fn poll_accept(&self, cx: &mut Context) -> Poll<io::Result<(Async<TcpStream>, SocketAddr)>> {
        // Safety: accept() is I/O safe
        unsafe {
            self.poll_event(Interest::Read, cx, |inner| {
                inner
                    .accept()
                    .and_then(|(st, addr)| Async::new(st).map(|st| (st, addr)))
            })
        }
    }

    /// Accept a new incoming TCP connection from this listener
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::net::TcpListener;
    /// use local_runtime::io::Async;
    ///
    /// # local_runtime::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// let (stream, addr) = listener.accept().await?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub async fn accept(&self) -> io::Result<(Async<TcpStream>, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// Return a stream of incoming TCP connections
    ///
    /// The returned stream will never return `None`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::net::TcpListener;
    /// use local_runtime::io::Async;
    /// use futures_lite::StreamExt;
    ///
    /// # local_runtime::block_on(async {
    /// let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 0))?;
    /// let mut incoming = listener.incoming();
    /// while let Some(stream) = incoming.next().await {
    ///     let stream = stream?;
    /// }
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn incoming(&self) -> IncomingTcp {
        IncomingTcp { listener: self }
    }
}

/// Stream returned by [`Async::<TcpListener>::incoming`]
#[must_use = "Streams do nothing unless polled"]
pub struct IncomingTcp<'a> {
    listener: &'a Async<TcpListener>,
}

impl Stream for IncomingTcp<'_> {
    type Item = io::Result<Async<TcpStream>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.listener
            .poll_accept(cx)
            .map(|pair| pair.map(|(st, _)| st))
            .map(Some)
    }
}

impl Async<TcpStream> {
    /// Create a TCP connection to the specified address
    ///
    /// ```no_run
    /// use std::net::TcpStream;
    /// use local_runtime::io::Async;
    ///
    /// # local_runtime::block_on(async {
    /// let listener = Async::<TcpStream>::connect(([127, 0, 0, 1], 8000)).await?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub async fn connect<A: Into<SocketAddr>>(addr: A) -> io::Result<Self> {
        let addr = addr.into();
        let stream = Async::without_nonblocking(tcp_socket(&addr)?);

        // Initiate the connection
        connect(&stream.inner, &addr)?;
        // Wait for the stream to be writable
        stream.wait_for_event_ready(Interest::Write).await;
        // Check for errors
        stream.inner.peer_addr()?;
        Ok(stream)
    }

    /// Reads data from the stream without removing it from the buffer.
    ///
    /// Returns the number of bytes read. Successive calls of this method read the same data.
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Safety: peek() is I/O safe
        unsafe { poll_fn(|cx| self.poll_event(Interest::Read, cx, |inner| inner.peek(buf))).await }
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let socket = socket_with(
        af,
        type_,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )?;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let socket = {
        let socket = socket_with(af, type_, SocketFlags::empty(), None)?;
        let previous = rustix::fs::fcntl_getfl(&socket)?;
        let new = previous | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC;
        if new != previous {
            rustix::fs::fcntl_setfl(&socket, new)?;
        }
        socket
    };

    Ok(socket.into())
}

#[cfg(unix)]
fn connect(tcp: &TcpStream, addr: &SocketAddr) -> io::Result<()> {
    match rustix::net::connect(tcp.as_fd(), addr) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::WOULDBLOCK) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, io::stderr, pin::pin, sync::Arc, time::Duration};

    use rustix::pipe::pipe;

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
            .poll(&mut Context::from_waker(&connect_waker.clone().into()))
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

    #[test]
    fn writable_readable() {
        let wr_waker = Arc::new(MockWaker::default());
        let rd_waker = Arc::new(MockWaker::default());

        let (read, write) = pipe().unwrap();
        set_nonblocking(read.as_fd()).unwrap();
        set_nonblocking(write.as_fd()).unwrap();

        let reader = Async::new(read).unwrap();
        let writer = Async::new(write).unwrap();

        let mut writable = pin!(writer.writable());
        assert!(writable
            .as_mut()
            .poll(&mut Context::from_waker(&wr_waker.clone().into()))
            .is_pending());
        REACTOR
            .with(|r| r.wait(&Some(Duration::from_millis(1))))
            .unwrap();
        assert!(wr_waker.get());
        assert!(writable
            .as_mut()
            .poll(&mut Context::from_waker(&wr_waker.clone().into()))
            .is_ready());

        let mut readable = pin!(reader.readable());
        assert!(readable
            .as_mut()
            .poll(&mut Context::from_waker(&wr_waker.clone().into()))
            .is_pending());
        REACTOR
            .with(|r| r.wait(&Some(Duration::from_millis(1))))
            .unwrap();
        assert!(!rd_waker.get());

        assert!(readable
            .as_mut()
            .poll(&mut Context::from_waker(&rd_waker.clone().into()))
            .is_pending());
        // Write one byte to pipe
        unsafe {
            assert!(writer
                .poll_write_with(&mut Context::from_waker(&rd_waker.clone().into()), |w| {
                    rustix::io::write(w, &[0]).map_err(Into::into)
                })
                .is_ready());
        };
        REACTOR
            .with(|r| r.wait(&Some(Duration::from_millis(1))))
            .unwrap();
        assert!(rd_waker.get());
        assert!(readable
            .as_mut()
            .poll(&mut Context::from_waker(&rd_waker.clone().into()))
            .is_ready());
    }
}
