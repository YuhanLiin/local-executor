use std::{
    cell::RefCell,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
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

pub struct Interest {
    read: bool,
    write: bool,
}

impl Interest {
    pub fn read() -> Self {
        Self {
            read: true,
            write: false,
        }
    }

    pub fn write() -> Self {
        Self {
            read: false,
            write: true,
        }
    }

    pub fn both() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
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

pub struct Reactor<N: Notifier, T: Timeout> {
    notifier: Arc<FlagNotifier<N>>,
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

impl<N: Notifier, T: Timeout> Reactor<N, T> {
    fn new() -> io::Result<Self> {
        let notifier = Arc::new(FlagNotifier::new(Notifier::new()?));
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
            self.notifier.inner.register(&mut inner.0.pollfds);
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
                // Now that we have awaken from the poll call, there's no need to send any
                // notifications to "wake up" from the poll, so we set the notified flag to prevent
                // our wakers from sending any notifications.
                self.notifier.set_to_notified();
                // For every FD that received an event, invoke its waker
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

    fn notifier(&self) -> Weak<FlagNotifier<N>> {
        Arc::downgrade(&self.notifier)
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
    fd: OwnedFd,
}

impl Notifier for EventFd {
    fn new() -> io::Result<Self> {
        let eventfd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(Self { fd: eventfd })
    }

    fn clear(&self) -> io::Result<()> {
        // Sets eventfd to 0
        io::read(&self.fd, &mut [0u8; 8]).map(drop)
    }

    fn notify(&self) -> io::Result<()> {
        // Eventfd should write all 8 bytes in a single call
        io::write(&self.fd, &1u64.to_ne_bytes()).map(drop)
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.fd.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

struct FlagNotifier<N: Notifier> {
    inner: N,
    is_notified: AtomicBool,
}

impl<N: Notifier> FlagNotifier<N> {
    fn new(inner: N) -> Self {
        Self {
            inner,
            is_notified: AtomicBool::new(false),
        }
    }

    fn notify(&self) -> io::Result<()> {
        // Use atomic flag to ensure that the inner notifier will only be called once even with
        // multiple notify() calls. Acquire memory order is used to ensure operations on the inner
        // notifier happen after checking the atomic flag.
        if self
            .is_notified
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            self.inner.notify()?;
        }
        Ok(())
    }

    fn clear(&self) -> io::Result<()> {
        let res = self.inner.clear();
        // Release memory ordering is used to ensure the inner notifier is cleared before clearing
        // the atomic flag.
        self.is_notified.store(false, Ordering::Release);
        res
    }

    fn set_to_notified(&self) {
        self.is_notified.store(true, Ordering::Relaxed);
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
                    .map(|d| d.subsec_nanos().max(1).into())
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

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        task::Wake,
        time::Instant,
    };

    use rustix::io::read;

    use super::*;

    macro_rules! assert_reactor_wait {
        ($reactor:ident, $timeout:expr) => {{
            let res = $reactor.wait($timeout);
            let inner = $reactor.inner.borrow();
            assert!(inner.pollfds.is_empty());
            assert!(inner.wakers.is_empty());
            res
        }};
    }

    #[test]
    fn only_eventfd() {
        let reactor = Reactor::<EventFd, PollTimeout>::new().unwrap();
        let notifier = reactor.notifier();

        std::thread::scope(|s| {
            s.spawn(move || {
                assert_reactor_wait!(reactor, None).unwrap();
            });
            notifier.upgrade().unwrap().notify().unwrap();
        });
    }

    #[test]
    fn poll_timeout() {
        let reactor = Reactor::<EventFd, PollTimeout>::new().unwrap();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_nanos(10))).unwrap();
        // Expect time to round up to nearest millisecond
        assert!(start.elapsed() >= Duration::from_millis(1));
    }

    #[test]
    fn timerfd_timeout() {
        let reactor = Reactor::<EventFd, TimerFd>::new().unwrap();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_reactor_wait!(reactor, Some(Duration::from_nanos(10))).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_nanos(10) && elapsed < Duration::from_millis(1));
    }

    #[derive(Default)]
    struct MockWaker(AtomicBool);
    impl Wake for MockWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn multiple_events() {
        const COUNT: usize = 5;
        let reactor = Reactor::<EventFd, PollTimeout>::new().unwrap();
        let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
        let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

        // Register 5 events and their respective wakers
        for (ev, wk) in events.iter().zip(&wakers) {
            unsafe { reactor.register(&ev.fd, Interest::read(), wk.clone().into()) };
        }

        events[0].notify().unwrap();
        events[2].notify().unwrap();
        events[4].notify().unwrap();
        assert_reactor_wait!(reactor, None).unwrap();

        for (i, wk) in wakers.iter().enumerate() {
            let awoken = wk.0.load(Ordering::Relaxed);
            match i {
                0 | 2 | 4 => assert!(awoken),
                _ => assert!(!awoken),
            }
        }
    }

    #[test]
    fn multiple_wakes() {
        const COUNT: usize = 5;
        let reactor = Reactor::<EventFd, PollTimeout>::new().unwrap();
        let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
        let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

        for i in [0, 1, 4] {
            // Register 5 events and their respective wakers
            for (ev, wk) in events.iter().zip(&wakers) {
                unsafe { reactor.register(&ev.fd, Interest::read(), wk.clone().into()) };
            }
            events[i].notify().unwrap();
            assert_reactor_wait!(reactor, None).unwrap();
            assert!(wakers[i].0.load(Ordering::Relaxed));
        }

        assert!(!wakers[2].0.load(Ordering::Relaxed));
        assert!(!wakers[3].0.load(Ordering::Relaxed));
    }

    #[test]
    fn flag_notifier() {
        let notifier = FlagNotifier::new(EventFd::new().unwrap());

        // Send 10 notifications simultaneously
        std::thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| notifier.notify());
            }
        });

        let mut eventfd_value = [0u8; 8];
        read(&notifier.inner.fd, &mut eventfd_value).unwrap();
        // The inner eventfd should have only been written once
        assert_eq!(u64::from_ne_bytes(eventfd_value), 1);
        assert!(notifier.is_notified.load(Ordering::Relaxed));
    }
}
