use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    sync::Arc,
    time::Duration,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::event::{eventfd, EventfdFlags};
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::time::{
    timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags,
    Timespec,
};
use rustix::{
    event::{poll, PollFd, PollFlags},
    pipe::pipe,
};

use crate::io::set_nonblocking;

use super::{EventNotifier, EventPoller, Filter, Source, WithFlag};

fn read_flags() -> PollFlags {
    PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::PRI
}

fn write_flags() -> PollFlags {
    PollFlags::OUT | PollFlags::HUP | PollFlags::ERR
}

impl Filter {
    fn pollflags(self) -> PollFlags {
        let mut flags = PollFlags::empty();
        if self.read {
            flags |= read_flags();
        }
        if self.write {
            flags |= write_flags();
        }
        flags
    }
}

pub(crate) struct PollPoller<N, T> {
    // All the pollfds will be constructed from raw fds, so don't worry about lifetimes
    pollfds: Vec<PollFd<'static>>,
    notifier: Arc<WithFlag<N>>,
    timeout: T,
}

impl<N: NotifierFd, T: Timeout> EventPoller for PollPoller<N, T> {
    type Notifier = N;

    fn new() -> io::Result<(Self, Arc<WithFlag<N>>)>
    where
        Self: Sized,
    {
        let notifier = Arc::new(WithFlag::new(N::new()?));
        let notifier_cl = notifier.clone();
        Ok((
            Self {
                pollfds: vec![],
                notifier,
                timeout: T::new()?,
            },
            notifier_cl,
        ))
    }

    unsafe fn register(&mut self, _source: Source) -> io::Result<()> {
        Ok(())
    }

    fn modify(&mut self, _source: Source, _filter: Filter) -> io::Result<()> {
        Ok(())
    }

    fn deregister(&mut self, _source: Source) -> io::Result<()> {
        Ok(())
    }

    fn poll(
        &mut self,
        timeout: Option<Duration>,
        event_sources: impl Iterator<Item = (Source, Filter)>,
    ) -> io::Result<Option<impl Iterator<Item = (Source, Filter)> + '_>> {
        self.pollfds.clear();
        // Set timeout as early as possible so that the timerfd starts ticking earlier
        let poll_timeout = self.timeout.set_timeout(timeout)?;

        for (source, filter) in event_sources {
            let pollflags = filter.pollflags();
            if !pollflags.is_empty() {
                // SAFETY: pollfds will be cleared by the end of the call
                let fd = unsafe { BorrowedFd::borrow_raw(source) };
                self.pollfds.push(PollFd::from_borrowed_fd(fd, pollflags));
            }
        }
        let event_len = self.pollfds.len();

        // SAFETY: pollfds will be cleared by the end of the call
        unsafe {
            self.pollfds.push(PollFd::from_borrowed_fd(
                BorrowedFd::borrow_raw(self.notifier.inner.as_raw_fd()),
                read_flags(),
            ));
            if let Some(fd) = self.timeout.maybe_fd() {
                self.pollfds.push(PollFd::from_borrowed_fd(
                    BorrowedFd::borrow_raw(fd),
                    read_flags(),
                ));
            }
        }

        log::trace!(
            "{:?} Reactor polling {} event sources with timeout of {} microseconds",
            std::thread::current().id(),
            event_len,
            if let Some(t) = timeout {
                t.as_micros() as i128
            } else {
                -1
            }
        );

        let poll_res = poll(&mut self.pollfds, poll_timeout)?;

        let out = match poll_res {
            // If poll timed out, don't bother checking the wakers
            0 => None,
            // If the only events received are the ones without a waker, then skip the waker check
            n @ 1 | n @ 2
                if self.pollfds[event_len..]
                    .iter()
                    .filter(|pfd| !pfd.revents().is_empty())
                    .count()
                    == n =>
            {
                None
            }

            _ => Some(
                self.pollfds
                    .iter()
                    .filter(|pollfd| !pollfd.revents().is_empty())
                    .map(|pollfd| {
                        let source = pollfd.as_fd().as_raw_fd();
                        let filter = Filter {
                            read: pollfd.revents().intersects(read_flags()),
                            write: pollfd.revents().intersects(write_flags()),
                        };
                        (source, filter)
                    }),
            ),
        };
        // Notifier and pollfds should get cleared here via the drop guard
        Ok(out)
    }
}

trait NotifierFd: EventNotifier + AsRawFd {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
}

/// Linux eventfd for notifying the poller
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
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl EventNotifier for EventFd {
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
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Unix pipe for notifying the poller on non-Linux platforms
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
}

impl EventNotifier for PipeFd {
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
}

impl AsRawFd for PipeFd {
    fn as_raw_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }
}

/// Method of handling timeouts on the poller
pub(crate) trait Timeout {
    fn new() -> io::Result<Self>
    where
        Self: Sized;
    /// Return the desired poll timeout
    fn set_timeout(&self, duration: Option<Duration>) -> io::Result<i32>;

    fn maybe_fd(&self) -> Option<RawFd>;
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

    fn maybe_fd(&self) -> Option<RawFd> {
        None
    }
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

    fn maybe_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) type Poller = PollPoller<EventFd, TimerFd>;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) type Poller = PollPoller<PipeFd, PollTimeout>;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) type PollerNotifier = EventFd;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) type PollerNotifier = PipeFd;

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    macro_rules! assert_poller_wait {
        ($poller:ident, $timeout:expr) => {{
            $poller.poll($timeout, std::iter::empty())
        }};
        ($poller:ident, $timeout:expr, $iter:expr) => {{
            $poller.poll($timeout, $iter.into_iter())
        }};
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eventfd_notification() {
        let (mut poller, notifier) = PollPoller::<EventFd, PollTimeout>::new().unwrap();

        std::thread::scope(|s| {
            s.spawn(move || {
                //Make sure the notification is sent after the poller starts waiting
                std::thread::sleep(Duration::from_millis(10));
                notifier.notify().unwrap();
            });
            assert_poller_wait!(poller, None).unwrap();
        });

        //Now send notification before poller starts waiting
        poller.notifier.notify().unwrap();
        assert_poller_wait!(poller, None).unwrap();
    }

    #[test]
    fn pipe_notification() {
        let (mut poller, notifier) = PollPoller::<PipeFd, PollTimeout>::new().unwrap();

        std::thread::scope(|s| {
            s.spawn(move || {
                // Make sure the notification is sent after the poller starts waiting
                std::thread::sleep(Duration::from_millis(10));
                notifier.notify().unwrap();
            });
            assert_poller_wait!(poller, None).unwrap();
        });

        // Now send notification before poller starts waiting
        poller.notifier.notify().unwrap();
        assert_poller_wait!(poller, None).unwrap();
    }

    #[test]
    fn poll_timeout() {
        let (mut poller, _) = PollPoller::<PipeFd, PollTimeout>::new().unwrap();
        assert_poller_wait!(poller, Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_poller_wait!(poller, Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_poller_wait!(poller, Some(Duration::from_nanos(10))).unwrap();
        // Expect time to round up to nearest millisecond
        assert!(start.elapsed() >= Duration::from_millis(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timerfd_timeout() {
        let (mut poller, _) = PollPoller::<EventFd, TimerFd>::new().unwrap();
        assert_poller_wait!(poller, Some(Duration::from_millis(0))).unwrap();

        let start = Instant::now();
        assert_poller_wait!(poller, Some(Duration::from_millis(10))).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));

        let start = Instant::now();
        assert_poller_wait!(poller, Some(Duration::from_nanos(10))).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_nanos(10) && elapsed < Duration::from_millis(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clear_notifier_and_timerfd() {
        let (mut poller, _) = PollPoller::<EventFd, PollTimeout>::new().unwrap();
        let efd = EventFd::new().unwrap();
        let timerfd = TimerFd::new().unwrap();
        let pipe = PipeFd::new().unwrap();
        let events = [
            (efd.fd.as_raw_fd(), Filter::read()),
            (timerfd.fd.as_raw_fd(), Filter::read()),
            (pipe.read.as_raw_fd(), Filter::read()),
        ];

        efd.notify().unwrap();
        pipe.notify().unwrap();
        timerfd.set_timeout(Some(Duration::ZERO)).unwrap();
        let revents = assert_poller_wait!(poller, Some(Duration::from_millis(10)), events).unwrap();
        assert_eq!(revents.unwrap().collect::<Vec<_>>(), &events);

        efd.clear().unwrap();
        pipe.clear().unwrap();
        timerfd.set_timeout(None).unwrap();
        let revents = assert_poller_wait!(poller, Some(Duration::from_millis(10)), events).unwrap();
        assert!(revents.is_none());

        // Make sure clear() doesn't error even without notification
        efd.clear().unwrap();
        pipe.clear().unwrap();
    }

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn multiple_events() {
    //const COUNT: usize = 5;
    //let poller = PollReactor::<EventFd, PollTimeout>::new().unwrap();
    //let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
    //let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

    //// Register 5 events and their respective wakers
    //for (ev, wk) in events.iter().zip(&wakers) {
    //let handle = unsafe { poller.register(&ev.fd) };
    //poller.enable_event(&handle, Interest::Read, &wk.clone().into());
    //}

    //events[0].notify().unwrap();
    //events[2].notify().unwrap();
    //events[4].notify().unwrap();
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();

    //for (i, wk) in wakers.iter().enumerate() {
    //let awoken = wk.get();
    //match i {
    //0 | 2 | 4 => assert!(awoken),
    //_ => assert!(!awoken),
    //}
    //}
    //}

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn multiple_wakes() {
    //const COUNT: usize = 5;
    //let poller = PollReactor::<EventFd, PollTimeout>::new().unwrap();
    //let events: Vec<_> = (0..COUNT).map(|_| EventFd::new().unwrap()).collect();
    //let wakers: Vec<_> = (0..COUNT).map(|_| Arc::new(MockWaker::default())).collect();

    //// Register 5 events and their respective wakers
    //for (ev, wk) in events.iter().zip(&wakers) {
    //let handle = unsafe { poller.register(&ev.fd) };
    //poller.enable_event(&handle, Interest::Read, &wk.clone().into());
    //}

    //for i in [0, 1, 4] {
    //events[i].notify().unwrap();
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(wakers[i].get());
    //}

    //assert!(!wakers[2].get());
    //assert!(!wakers[3].get());
    //}

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn modify_registration() {
    //let poller = PollReactor::<EventFd, PollTimeout>::new().unwrap();
    //let event = EventFd::new().unwrap();
    //let wakers: Vec<_> = (0..3).map(|_| Arc::new(MockWaker::default())).collect();

    //let handle = unsafe { poller.register(&event.fd) };
    //poller.enable_event(&handle, Interest::Read, &wakers[0].clone().into());
    //event.notify().unwrap();
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(wakers[0].get());

    //poller.enable_event(&handle, Interest::Read, &wakers[1].clone().into());
    //event.notify().unwrap();
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(wakers[1].get());

    //poller.enable_event(&handle, Interest::Read, &wakers[2].clone().into());
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(wakers[2].get());

    //poller.deregister(&handle);
    //assert!(poller.inner.borrow().event_sources.is_empty());
    //}

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn repeated_source() {
    //let poller = PollReactor::<EventFd, PollTimeout>::new().unwrap();
    //let event = EventFd::new().unwrap();
    //let wakers: Vec<_> = (0..3).map(|_| Arc::new(MockWaker::default())).collect();

    //let id1 = unsafe { poller.register(&event.fd) };
    //let id2 = unsafe { poller.register(&event.fd) };
    //let id3 = unsafe { poller.register(&event.fd) };
    //poller.enable_event(&id1, Interest::Read, &wakers[0].clone().into());
    //poller.enable_event(&id2, Interest::Read, &wakers[1].clone().into());
    //poller.enable_event(&id3, Interest::Write, &wakers[2].clone().into());

    //event.notify().unwrap();
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(wakers[0].get());
    //assert!(wakers[1].get());
    //assert!(wakers[2].get());

    //for wk in &wakers {
    //wk.set(false);
    //}
    //poller.deregister(&id1);
    //poller.deregister(&id3);
    //poller.enable_event(&id2, Interest::Read, &wakers[1].clone().into());

    //event.notify().unwrap();
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(!wakers[0].get());
    //assert!(wakers[1].get());
    //assert!(!wakers[2].get());
    //}

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn disable_interest_after_poll() {
    //let poller = PollReactor::<EventFd, PollTimeout>::new().unwrap();
    //let event = EventFd::new().unwrap();
    //let wakers: Vec<_> = (0..2).map(|_| Arc::new(MockWaker::default())).collect();

    //let handle = unsafe { poller.register(&event.fd) };
    //poller.enable_event(&handle, Interest::Read, &wakers[0].clone().into());
    //poller.enable_event(&handle, Interest::Write, &wakers[1].clone().into());

    //// Only the write event should fire
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(!wakers[0].get());
    //assert!(wakers[1].get());

    //wakers[1].set(false);
    //event.notify().unwrap();
    //// poller is not longer waiting on the write event, so only read event should fire
    //assert_poller_wait!(poller, &Some(Duration::from_secs(1))).unwrap();
    //assert!(wakers[0].get());
    //assert!(!wakers[1].get());
    //}

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn id_overflow() {
    //let poller = PollReactor::<EventFd, PollTimeout>::new().unwrap();
    //let event = EventFd::new().unwrap();

    //let handle = unsafe { poller.register(&event.fd) };
    //assert_eq!(handle.id.0.get(), 1);

    //poller.current_id.set(Id::new(usize::MAX));
    //// This ID will be usize::MAX
    //unsafe { poller.register(&event.fd) };
    //// This ID should be 2, not 1
    //let handle = unsafe { poller.register(&event.fd) };
    //assert_eq!(handle.id.0.get(), 2);
    //}

    //#[cfg(target_os = "linux")]
    //#[test]
    //fn flag_notifier() {
    //let notifier = FlagNotifier::new(EventFd::new().unwrap());

    //// Send 10 notifications simultaneously
    //std::thread::scope(|s| {
    //for _ in 0..10 {
    //s.spawn(|| notifier.notify());
    //}
    //});

    //let mut eventfd_value = [0u8; 8];
    //read(&notifier.inner.fd, &mut eventfd_value).unwrap();
    //// The inner eventfd should have only been written once
    //assert_eq!(u64::from_ne_bytes(eventfd_value), 1);
    //assert!(notifier.is_notified.load(Ordering::Relaxed));
    //}
}
