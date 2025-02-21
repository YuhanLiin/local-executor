use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    task::Waker,
    time::Duration,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::{
    event::{eventfd, EventfdFlags},
    time::{
        timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags,
        TimerfdTimerFlags, Timespec,
    },
};
use rustix::{
    event::{poll, PollFd, PollFlags},
    pipe::pipe,
};

use crate::{io::set_nonblocking, Id};

use super::{EventHandle, Interest, Notifier, Reactor, TimeoutProvider};

fn read_flags() -> PollFlags {
    PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::PRI
}

fn write_flags() -> PollFlags {
    PollFlags::OUT | PollFlags::HUP | PollFlags::ERR
}

/// Reactor that uses `poll()` to wait for events, making it compatible on all Unix platforms.
pub(crate) struct PollReactor<N: NotifierFd, T: Timeout> {
    current_id: Cell<Id>,
    inner: RefCell<Inner>,
    notifier: Arc<FlagNotifier<N>>,
    timeout: T,
}

#[derive(Default)]
struct Wakeup {
    enabled: bool,
    waker: Option<Waker>,
}

impl Wakeup {
    fn wake(&mut self) {
        // Disable the wakeup after waking up, to prevent spurious wakeups in the future
        if let (true, Some(waker)) = (self.enabled, &self.waker) {
            waker.wake_by_ref();
            self.enabled = false;
        }
    }

    fn enable(&mut self, waker: &Waker) {
        self.enabled = true;
        match &mut self.waker {
            Some(wk) => wk.clone_from(waker),
            None => self.waker = Some(waker.clone()),
        }
    }
}

#[derive(Default)]
struct EventData {
    read: Wakeup,
    write: Wakeup,
}

impl EventData {
    fn pollflags(&self) -> PollFlags {
        let mut flags = PollFlags::empty();
        if self.read.enabled {
            flags |= read_flags();
        }
        if self.write.enabled {
            flags |= write_flags();
        }
        flags
    }
}

// The part of reactor that requires interior mutability
#[derive(Default)]
struct Inner {
    // All the pollfds will be constructed from raw fds, so don't worry about lifetimes
    pollfds: Vec<PollFd<'static>>,
    ids: Vec<Id>,
    event_sources: BTreeMap<EventHandle, EventData>,
}

impl<N: NotifierFd + 'static, T: Timeout> Reactor for PollReactor<N, T> {
    type Notifier = FlagNotifier<N>;

    fn new() -> io::Result<Self> {
        let current_id = Cell::new(const { Id::new(1) });
        let notifier = Arc::new(FlagNotifier::new(NotifierFd::new()?));
        let timeout = Timeout::new()?;
        let inner = Inner::default();
        Ok(Self {
            current_id,
            inner: RefCell::new(inner),
            notifier,
            timeout,
        })
    }

    unsafe fn register<S: AsFd>(&self, source: &S) -> EventHandle {
        let mut inner = self.inner.borrow_mut();
        let fd = source.as_fd().as_raw_fd();
        loop {
            let id = self.current_id.get();
            self.current_id.set(id.overflowing_incr());
            let handle = EventHandle { fd, id };
            // On the rare chance that the (ID, raw_fd) pair already exists, which can only happen
            // if the ID overflowed and the same FD is still registered on that ID, then just try
            // the next ID.
            match inner
                .event_sources
                .insert(EventHandle { ..handle }, EventData::default())
            {
                None => break handle,
                // Restore the previous event source
                Some(prev_data) => {
                    inner.event_sources.insert(handle, prev_data);
                }
            }
        }
    }

    fn deregister(&self, handle: &EventHandle) {
        if self
            .inner
            .borrow_mut()
            .event_sources
            .remove(handle)
            .is_none()
        {
            log::error!(
                "{:?} Deregistering non-existent event source {{ fd = {}, id = {} }}",
                std::thread::current().id(),
                handle.fd,
                handle.id.0
            );
        }
    }

    fn enable_event(&self, handle: &EventHandle, interest: Interest, waker: &Waker) {
        let mut inner = self.inner.borrow_mut();
        if let Some(entry) = inner.event_sources.get_mut(handle) {
            let dir = match interest {
                Interest::Read => &mut entry.read,
                Interest::Write => &mut entry.write,
            };
            dir.enable(waker);
        } else {
            log::error!(
                "{:?} Enabling non-existent event source {{ fd = {}, id = {} }}",
                std::thread::current().id(),
                handle.fd,
                handle.id.0
            );
        }
    }

    fn is_event_ready(&self, handle: &EventHandle, interest: Interest) -> bool {
        let inner = self.inner.borrow();
        if let Some(entry) = inner.event_sources.get(handle) {
            let dir = match interest {
                Interest::Read => &entry.read,
                Interest::Write => &entry.write,
            };
            !dir.enabled
        } else {
            log::error!(
                "{:?} Checking non-existent event source {{ fd = {}, id = {} }}",
                std::thread::current().id(),
                handle.fd,
                handle.id.0
            );
            false
        }
    }

    fn wait<TO: TimeoutProvider>(&self, timeout_provider: &TO) -> io::Result<()> {
        // Drop guard to ensure the pollfds and notifier are always cleared
        struct DropGuard<'a, N: NotifierFd>(&'a mut Inner, &'a FlagNotifier<N>);
        impl<N: NotifierFd> Drop for DropGuard<'_, N> {
            fn drop(&mut self) {
                self.0.pollfds.clear();
                self.0.ids.clear();
                if let Err(err) = self.1.clear() {
                    log::error!(
                        "{:?} Error clearing notifications: {err}",
                        std::thread::current().id(),
                    );
                }
            }
        }

        let timeout = timeout_provider.next_timeout();
        let mut borrow = self.inner.borrow_mut();
        let inner = DropGuard(&mut borrow, self.notifier.as_ref());
        let poll_timeout = self.timeout.set_timeout(timeout)?;

        for (handle, data) in &inner.0.event_sources {
            let pollflags = data.pollflags();
            if !pollflags.is_empty() {
                // SAFETY: pollfds will be cleared by the end of the call
                let fd = unsafe { BorrowedFd::borrow_raw(handle.fd) };
                inner
                    .0
                    .pollfds
                    .push(PollFd::from_borrowed_fd(fd, pollflags));
                inner.0.ids.push(handle.id);
            }
        }

        // SAFETY: pollfds will be cleared by the end of the call
        unsafe {
            self.notifier.inner.register(&mut inner.0.pollfds);
            self.timeout.register(&mut inner.0.pollfds);
        }

        log::trace!(
            "{:?} Reactor polling {} event sources with timeout of {} microseconds",
            std::thread::current().id(),
            inner.0.ids.len(),
            if let Some(t) = timeout {
                t.as_micros() as i128
            } else {
                -1
            }
        );

        let poll_res = poll(&mut inner.0.pollfds, poll_timeout)?;
        // Now that we have awaken from the poll call, there's no need to send any
        // notifications to "wake up" from the poll, so we set the notified flag to prevent
        // our wakers from sending any notifications.
        self.notifier.set_to_notified();
        // Update timeout provider internal state, including any expired wakers
        timeout_provider.update();

        match poll_res {
            // If poll timed out, don't bother checking the wakers
            0 => {}

            // If the only events received are the ones without a waker, then skip the waker check
            n @ 1 | n @ 2
                if inner.0.pollfds[inner.0.ids.len()..]
                    .iter()
                    .filter(|pfd| !pfd.revents().is_empty())
                    .count()
                    == n => {}

            _ => {
                for (pollfd, id) in inner.0.pollfds.iter().zip(&inner.0.ids) {
                    if !pollfd.revents().is_empty() {
                        let handle = EventHandle {
                            id: *id,
                            fd: pollfd.as_fd().as_raw_fd(),
                        };
                        let data = inner.0.event_sources.get_mut(&handle).unwrap();
                        if pollfd.revents().intersects(read_flags()) {
                            data.read.wake();
                        }
                        if pollfd.revents().intersects(write_flags()) {
                            data.write.wake();
                        }
                    }
                }
            }
        };

        // Notifier and pollfds should get cleared here via the drop guard
        Ok(())
    }

    fn notifier(&self) -> Weak<Self::Notifier> {
        Arc::downgrade(&self.notifier)
    }

    fn clear_notifications(&self) {
        if let Err(err) = self.notifier.clear() {
            log::error!(
                "{:?} Error clearing notifications: {err}",
                std::thread::current().id(),
            );
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.inner.borrow().event_sources.is_empty()
    }
}

/// Method of notifying the reactor to wake it up
pub(crate) trait NotifierFd: 'static {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
    fn clear(&self) -> io::Result<()>;
    fn notify(&self) -> io::Result<()>;
    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>);
}

/// Linux eventfd for notifying the reactor
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) struct EventFd {
    fd: OwnedFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl NotifierFd for EventFd {
    fn new() -> io::Result<Self> {
        let eventfd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(Self { fd: eventfd })
    }

    fn clear(&self) -> io::Result<()> {
        // Sets eventfd to 0
        match rustix::io::read(&self.fd, &mut [0u8; 8]).map_err(io::Error::from) {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
            res => res.map(drop),
        }
    }

    fn notify(&self) -> io::Result<()> {
        // Eventfd should write all 8 bytes in a single call
        rustix::io::write(&self.fd, &1u64.to_ne_bytes()).map(drop)?;
        Ok(())
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.fd.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

/// Unix pipe for notifying the reactor on non-Linux platforms
#[allow(unused)]
pub(crate) struct PipeFd {
    read: OwnedFd,
    write: OwnedFd,
}

impl NotifierFd for PipeFd {
    fn new() -> io::Result<Self>
    where
        Self: Sized,
    {
        let (read, write) = pipe()?;
        set_nonblocking(read.as_fd())?;
        set_nonblocking(write.as_fd())?;
        Ok(Self { read, write })
    }

    fn clear(&self) -> io::Result<()> {
        // Ideally we want to clear every notification, but each notification requires one byte of
        // memory to read out. Since this pipe will be wrapped in a `FlagNotifier`, there shouldn't
        // be more than one notification written at a time, so reading 8 bytes should suffice.
        match rustix::io::read(&self.read, &mut [0u8; 8]).map_err(io::Error::from) {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
            res => res.map(drop),
        }
    }

    fn notify(&self) -> io::Result<()> {
        // Write one byte to the pipe
        rustix::io::write(&self.write, &[0])?;
        Ok(())
    }

    unsafe fn register(&self, pollfds: &mut Vec<PollFd<'static>>) {
        // Register the read end of the pipe
        pollfds.push(PollFd::from_borrowed_fd(
            BorrowedFd::borrow_raw(self.read.as_raw_fd()),
            PollFlags::IN,
        ));
    }
}

/// Wraps a `NotifierFd` implementation with an atomic flag so that the notification is only sent
/// once to the FD.
pub(crate) struct FlagNotifier<N: NotifierFd> {
    inner: N,
    is_notified: AtomicBool,
}

impl<N: NotifierFd> FlagNotifier<N> {
    fn new(inner: N) -> Self {
        Self {
            inner,
            is_notified: AtomicBool::new(false),
        }
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

impl<N: NotifierFd> Notifier for FlagNotifier<N> {
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
}

/// Method of handling timeouts on the reactor
pub(crate) trait Timeout {
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
#[allow(unused)]
pub(crate) struct PollTimeout;

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

/// Linux timerfd that can handle timeouts of nanosecond precision
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) struct TimerFd {
    fd: OwnedFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
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

// On Linux platforms, use eventfd for notification and timerfd for timeouts
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) type UnixReactor = PollReactor<EventFd, TimerFd>;
// On non-Linux platforms, use pipe for notification and the poll() argument for timeouts
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) type UnixReactor = PollReactor<PipeFd, PollTimeout>;

#[cfg(test)]
mod tests {
    use std::{sync::atomic::Ordering, time::Instant};

    use rustix::io::read;

    use super::*;
    use crate::{
        reactor::{Notifier, Reactor},
        test::MockWaker,
    };

    macro_rules! assert_reactor_wait {
        ($reactor:ident, $timeout:expr) => {{
            let res = $reactor.wait($timeout);
            let inner = $reactor.inner.borrow();
            assert!(inner.pollfds.is_empty());
            assert!(inner.ids.is_empty());
            res
        }};
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eventfd_notification() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let notifier = reactor.notifier();

        std::thread::scope(|s| {
            s.spawn(move || {
                // Make sure the notification is sent after the reactor starts waiting
                std::thread::sleep(Duration::from_millis(10));
                notifier.upgrade().unwrap().notify().unwrap();
            });
            assert_reactor_wait!(reactor, &None).unwrap();
        });

        // Now send notification before reactor starts waiting
        reactor.notifier().upgrade().unwrap().notify().unwrap();
        assert_reactor_wait!(reactor, &None).unwrap();
    }

    #[test]
    fn pipe_notification() {
        let reactor = PollReactor::<PipeFd, PollTimeout>::new().unwrap();
        let notifier = reactor.notifier();

        std::thread::scope(|s| {
            s.spawn(move || {
                // Make sure the notification is sent after the reactor starts waiting
                std::thread::sleep(Duration::from_millis(10));
                notifier.upgrade().unwrap().notify().unwrap();
            });
            assert_reactor_wait!(reactor, &None).unwrap();
        });

        // Now send notification before reactor starts waiting
        reactor.notifier().upgrade().unwrap().notify().unwrap();
        assert_reactor_wait!(reactor, &None).unwrap();
    }

    #[test]
    fn poll_timeout() {
        let reactor = PollReactor::<PipeFd, PollTimeout>::new().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_reactor_wait!(reactor, &Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_reactor_wait!(reactor, &Some(Duration::from_nanos(10))).unwrap();
        // Expect time to round up to nearest millisecond
        assert!(start.elapsed() >= Duration::from_millis(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timerfd_timeout() {
        let reactor = PollReactor::<EventFd, TimerFd>::new().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_reactor_wait!(reactor, &Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_reactor_wait!(reactor, &Some(Duration::from_nanos(10))).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_nanos(10) && elapsed < Duration::from_millis(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clear_notifier_and_timerfd() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let waker = Arc::new(MockWaker::default());
        let efd = EventFd::new().unwrap();
        let timerfd = TimerFd::new().unwrap();
        let pipe = PipeFd::new().unwrap();

        let handle = unsafe { reactor.register(&efd.fd) };
        reactor.enable_event(&handle, Interest::Read, &waker.clone().into());
        let handle = unsafe { reactor.register(&timerfd.fd) };
        reactor.enable_event(&handle, Interest::Read, &waker.clone().into());
        let handle = unsafe { reactor.register(&pipe.read) };
        reactor.enable_event(&handle, Interest::Read, &waker.clone().into());

        efd.notify().unwrap();
        pipe.notify().unwrap();
        timerfd.set_timeout(Some(Duration::ZERO)).unwrap();
        efd.clear().unwrap();
        pipe.clear().unwrap();
        timerfd.set_timeout(None).unwrap();

        reactor.wait(&Some(Duration::from_millis(10))).unwrap();
        // Check that none of the event sources actually fired
        assert!(!waker.get());

        // Make sure clear() doesn't error even without notification
        efd.clear().unwrap();
        pipe.clear().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn multiple_events() {
        const COUNT: usize = 5;
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
        let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

        // Register 5 events and their respective wakers
        for (ev, wk) in events.iter().zip(&wakers) {
            let handle = unsafe { reactor.register(&ev.fd) };
            reactor.enable_event(&handle, Interest::Read, &wk.clone().into());
        }

        events[0].notify().unwrap();
        events[2].notify().unwrap();
        events[4].notify().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();

        for (i, wk) in wakers.iter().enumerate() {
            let awoken = wk.get();
            match i {
                0 | 2 | 4 => assert!(awoken),
                _ => assert!(!awoken),
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn multiple_wakes() {
        const COUNT: usize = 5;
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
        let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

        // Register 5 events and their respective wakers
        for (ev, wk) in events.iter().zip(&wakers) {
            let handle = unsafe { reactor.register(&ev.fd) };
            reactor.enable_event(&handle, Interest::Read, &wk.clone().into());
        }

        for i in [0, 1, 4] {
            events[i].notify().unwrap();
            assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
            assert!(wakers[i].get());
        }

        assert!(!wakers[2].get());
        assert!(!wakers[3].get());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn modify_registration() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let event = EventFd::new().unwrap();
        let wakers: Vec<_> = (0..3).map(|_| Arc::new(MockWaker::default())).collect();

        let handle = unsafe { reactor.register(&event.fd) };
        reactor.enable_event(&handle, Interest::Read, &wakers[0].clone().into());
        event.notify().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(wakers[0].get());

        reactor.enable_event(&handle, Interest::Read, &wakers[1].clone().into());
        event.notify().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(wakers[1].get());

        reactor.enable_event(&handle, Interest::Read, &wakers[2].clone().into());
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(wakers[2].get());

        reactor.deregister(&handle);
        assert!(reactor.inner.borrow().event_sources.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn repeated_source() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let event = EventFd::new().unwrap();
        let wakers: Vec<_> = (0..3).map(|_| Arc::new(MockWaker::default())).collect();

        let id1 = unsafe { reactor.register(&event.fd) };
        let id2 = unsafe { reactor.register(&event.fd) };
        let id3 = unsafe { reactor.register(&event.fd) };
        reactor.enable_event(&id1, Interest::Read, &wakers[0].clone().into());
        reactor.enable_event(&id2, Interest::Read, &wakers[1].clone().into());
        reactor.enable_event(&id3, Interest::Write, &wakers[2].clone().into());

        event.notify().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(wakers[0].get());
        assert!(wakers[1].get());
        assert!(wakers[2].get());

        for wk in &wakers {
            wk.set(false);
        }
        reactor.deregister(&id1);
        reactor.deregister(&id3);
        reactor.enable_event(&id2, Interest::Read, &wakers[1].clone().into());

        event.notify().unwrap();
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(!wakers[0].get());
        assert!(wakers[1].get());
        assert!(!wakers[2].get());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disable_interest_after_poll() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let event = EventFd::new().unwrap();
        let wakers: Vec<_> = (0..2).map(|_| Arc::new(MockWaker::default())).collect();

        let handle = unsafe { reactor.register(&event.fd) };
        reactor.enable_event(&handle, Interest::Read, &wakers[0].clone().into());
        reactor.enable_event(&handle, Interest::Write, &wakers[1].clone().into());

        // Only the write event should fire
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(!wakers[0].get());
        assert!(wakers[1].get());

        wakers[1].set(false);
        event.notify().unwrap();
        // Reactor is not longer waiting on the write event, so only read event should fire
        assert_reactor_wait!(reactor, &Some(Duration::from_secs(1))).unwrap();
        assert!(wakers[0].get());
        assert!(!wakers[1].get());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn id_overflow() {
        let reactor = PollReactor::<EventFd, PollTimeout>::new().unwrap();
        let event = EventFd::new().unwrap();

        let handle = unsafe { reactor.register(&event.fd) };
        assert_eq!(handle.id.0.get(), 1);

        reactor.current_id.set(Id::new(usize::MAX));
        // This ID will be usize::MAX
        unsafe { reactor.register(&event.fd) };
        // This ID should be 2, not 1
        let handle = unsafe { reactor.register(&event.fd) };
        assert_eq!(handle.id.0.get(), 2);
    }

    #[cfg(target_os = "linux")]
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
