use std::{
    cell::RefCell,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    task::Waker,
    time::Duration,
};

use rustix::{
    event::{eventfd, poll, EventfdFlags, PollFd, PollFlags},
    io,
    time::{
        timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags,
        TimerfdTimerFlags, Timespec,
    },
};

pub struct Reactor<N: Notifier, T: Timeout> {
    notifier: N,
    timeout: T,
    inner: RefCell<Inner>,
}

// The part of reactor that requires interior mutability
#[derive(Default)]
struct Inner {
    // All the pollfds will be constructed from raw fds, so don't worry about lifetimes
    pollfds: Vec<PollFd<'static>>,
    wakers: Vec<Waker>,
}

pub struct Interest {
    read: bool,
    write: bool,
}

impl From<Interest> for PollFlags {
    fn from(val: Interest) -> Self {
        let mut flags = PollFlags::empty();
        if val.read {
            flags |= PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::PRI;
        }
        if val.write {
            flags |= PollFlags::OUT | PollFlags::HUP | PollFlags::ERR;
        }
        flags
    }
}

impl<N: Notifier, T: Timeout> Reactor<N, T> {
    fn new() -> io::Result<Self> {
        let notifier = Notifier::new()?;
        let timeout = Timeout::new()?;
        let inner = Inner::default();
        Ok(Self {
            notifier,
            timeout,
            inner: RefCell::new(inner),
        })
    }

    unsafe fn register<S: AsRawFd>(&self, source: &S, interest: Interest, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        let fd = BorrowedFd::borrow_raw(source.as_raw_fd());
        inner
            .pollfds
            .push(PollFd::from_borrowed_fd(fd, interest.into()));
        inner.wakers.push(waker);
    }
    fn wait(&self, timeout: Option<Duration>) -> io::Result<()> {
        // Drop guard to ensure the pollfds and wakers are always cleared
        struct InnerGuard<'a>(&'a mut Inner);
        impl Drop for InnerGuard<'_> {
            fn drop(&mut self) {
                self.0.pollfds.clear();
                self.0.wakers.clear();
            }
        }

        let mut borrow = self.inner.borrow_mut();
        let inner = InnerGuard(&mut borrow);

        let timeout = self.timeout.set_timeout(timeout)?;
        // SAFETY: pollfds will be cleared by the end of the call
        unsafe {
            self.notifier.register(&mut inner.0.pollfds);
            self.timeout.register(&mut inner.0.pollfds);
        }

        match poll(&mut inner.0.pollfds, timeout)? {
            // If poll timed out, don't bother checking the wakers
            0 => {}
            // If the only events received are the ones without a waker, then skip the waker check
            n @ 1 | n @ 2
                if inner.0.pollfds[inner.0.wakers.len()..]
                    .iter()
                    .filter(|pfd| !pfd.revents().is_empty())
                    .count()
                    == n => {}

            _ => {
                for (pollfd, waker) in inner.0.pollfds.iter().zip(&inner.0.wakers) {
                    if pollfd.revents().intersects(
                        PollFlags::IN
                            | PollFlags::OUT
                            | PollFlags::HUP
                            | PollFlags::ERR
                            | PollFlags::PRI,
                    ) {
                        waker.wake_by_ref();
                    }
                }
            }
        };

        let _ = self.notifier.clear();
        Ok(())
    }

    fn notify(&self) -> io::Result<()> {
        self.notifier.notify()
    }
}

trait Notifier {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
    fn clear(&self) -> io::Result<()>;
    fn notify(&self) -> io::Result<()>;
    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>);
}

struct EventFd {
    eventfd: OwnedFd,
}

impl Notifier for EventFd {
    fn new() -> io::Result<Self> {
        let eventfd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(Self { eventfd })
    }

    fn clear(&self) -> io::Result<()> {
        // Sets eventfd to 0
        io::read(&self.eventfd, &mut [0u8; 8]).map(drop)
    }

    fn notify(&self) -> io::Result<()> {
        // Eventfd should write all 8 bytes in a single call
        io::write(&self.eventfd, &1u64.to_ne_bytes()).map(drop)
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.eventfd.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

trait Timeout {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
    /// Return the desired poll timeout
    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32>;
    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>);
}

/// Use the timeout argument of poll() to handle timers
///
/// Limited to only millisecond precision
struct PollTimeout;

impl Timeout for PollTimeout {
    fn new() -> io::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self)
    }

    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32> {
        // Round duration up to nearest millisecond, or -1 if there's no timeout
        let timeout = duration
            .map(|d| {
                d.as_millis()
                    .try_into()
                    .unwrap_or(i32::MAX)
                    .saturating_add(if d.as_nanos() > 0 { 1 } else { 0 })
            })
            .unwrap_or(-1);
        Ok(timeout)
    }

    unsafe fn register(&self, _pollfds: &mut Vec<PollFd<'static>>) {}
}

struct TimerFd {
    fd: OwnedFd,
}

impl Timeout for TimerFd {
    fn new() -> io::Result<Self>
    where
        Self: Sized,
    {
        let fd = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )?;
        Ok(Self { fd })
    }

    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32> {
        let itimerspec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: duration
                    .map(|d| d.as_secs().try_into().unwrap_or(i64::MAX))
                    .unwrap_or(0),
                // If duration is 0, then the timespec needs to be at least 1 nanosecond, since
                // setting timespec to 0 disarms the timerfd
                tv_nsec: duration
                    .map(|d| d.subsec_nanos().min(1).into())
                    .unwrap_or(0),
            },
        };
        timerfd_settime(&self.fd, TimerfdTimerFlags::empty(), &itimerspec)?;
        Ok(-1)
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.fd.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}
