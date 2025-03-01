mod unix;

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Waker,
    time::{Duration, Instant},
};

use crate::{time::TimerQueue, Id};

/// Type of event that we're interested in receiving
#[derive(Debug, Clone, Copy)]
pub(crate) enum Interest {
    Read,
    Write,
}

#[cfg(unix)]
pub(crate) type Source = std::os::fd::RawFd;
#[cfg(not(unix))]
compile_error!("Unsupported operating system!");

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Filter {
    read: bool,
    write: bool,
}

impl Filter {
    #[cfg(test)]
    fn read() -> Self {
        Self {
            read: true,
            write: false,
        }
    }

    #[cfg(test)]
    fn write() -> Self {
        Self {
            read: false,
            write: true,
        }
    }

    #[cfg(test)]
    fn both() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}

/// Method of notifying the reactor to wake it up
trait EventNotifier: 'static {
    fn clear(&self) -> io::Result<()>;
    fn notify(&self) -> io::Result<()>;
}

trait EventPoller {
    type Notifier: EventNotifier;

    fn new() -> io::Result<(Self, Arc<WithFlag<Self::Notifier>>)>
    where
        Self: Sized;

    /// SAFETY: The event source must not be dropped before it's cleared from the reactor via
    /// `deregister()`.
    unsafe fn register(&mut self, source: Source) -> io::Result<()>;

    fn modify(&mut self, source: Source, filter: Filter) -> io::Result<()>;

    fn deregister(&mut self, source: Source) -> io::Result<()>;

    fn poll(
        &mut self,
        timeout: Option<Duration>,
        event_sources: impl Iterator<Item = (Source, Filter)>,
    ) -> io::Result<Option<impl Iterator<Item = (Source, Filter)> + '_>>;
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
    fn filter(&self) -> Filter {
        Filter {
            read: self.read.enabled,
            write: self.write.enabled,
        }
    }
}

/// Wraps a `EventNotifier` implementation with an atomic flag so that the notification is only
/// sent once to the FD.
#[derive(Debug, Default)]
pub(crate) struct WithFlag<N> {
    inner: N,
    is_notified: AtomicBool,
}

#[allow(private_bounds)]
impl<N: EventNotifier> WithFlag<N> {
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

    fn is_notified(&self) -> bool {
        self.is_notified.load(Ordering::Relaxed)
    }

    pub(crate) fn notify(&self) -> io::Result<()> {
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

struct State<P> {
    poller: P,
    event_sources: BTreeMap<Source, EventData>,
    timer_queue: TimerQueue,
}

#[allow(private_bounds)]
pub(crate) struct Reactor<P: EventPoller> {
    state: RefCell<State<P>>,
    notifier: Arc<WithFlag<P::Notifier>>,
}

/// General trait for the reactor used to wakeup futures
#[allow(private_bounds)]
impl<P: EventPoller> Reactor<P> {
    /// Construct new reactor
    pub(crate) fn new() -> io::Result<Self> {
        let (poller, notifier) = P::new()?;
        Ok(Reactor {
            state: RefCell::new(State {
                poller,
                event_sources: BTreeMap::new(),
                timer_queue: TimerQueue::new(),
            }),
            notifier,
        })
    }

    #[cfg(unix)]
    /// Register new event source onto the reactor along with a Waker that will be pinged if an
    /// event is received on the source. Returns an unique ID to that event source.
    ///
    /// SAFETY: The event source must not be dropped before it's cleared from the reactor via
    /// `deregister()`.
    pub(crate) unsafe fn register_event(&self, source: Source) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.poller.register(source)?;
        if let Some(prev) = state.event_sources.insert(source, EventData::default()) {
            // Restore previous entry
            state.event_sources.insert(source, prev);
            return Err(io::Error::other(
                "event source already registered in reactor",
            ));
        }
        Ok(())
    }

    /// Deregister event source from the reactor
    pub(crate) fn deregister_event(&self, source: Source) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        state.poller.deregister(source)?;
        state
            .event_sources
            .remove(&source)
            .expect("deregistering non-existent event source");
        Ok(())
    }

    /// Enable a registered event source.
    pub(crate) fn enable_event(
        &self,
        source: Source,
        interest: Interest,
        waker: &Waker,
    ) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        let event_data = state
            .event_sources
            .get_mut(&source)
            .expect("enabling non-existent event source");
        let dir = match interest {
            Interest::Read => &mut event_data.read,
            Interest::Write => &mut event_data.write,
        };
        dir.enable(waker);
        let filter = event_data.filter();
        state.poller.modify(source, filter)
    }

    /// Check if an event is ready since the last time `enable_event` was called
    pub(crate) fn is_event_ready(&self, source: Source, interest: Interest) -> bool {
        let state = self.state.borrow();
        let entry = state
            .event_sources
            .get(&source)
            .expect("checking non-existent event source");
        let dir = match interest {
            Interest::Read => &entry.read,
            Interest::Write => &entry.write,
        };
        !dir.enabled
    }

    /// Wait for an event on the reactor with an optional timeout, then clears all event sources.
    pub(crate) fn wait(&self) -> io::Result<()> {
        let state = &mut *self.state.borrow_mut();
        let timeout = state.timer_queue.next_timeout();

        if timeout == Some(Duration::ZERO) || self.notifier.is_notified() {
            log::trace!(
                "{:?} Skip polling events since wakeup will be instant",
                std::thread::current().id(),
            );
        } else {
            let event_sources = state.event_sources.iter().map(|(s, d)| (*s, d.filter()));
            let revents = state.poller.poll(timeout, event_sources)?;
            // Now that we have awaken from the poll call, there's no need to send any
            // notifications to "wake up" from the poll, so we set the notified flag to prevent
            // our wakers from sending any notifications.
            self.notifier.set_to_notified();

            for (source, filter) in revents.into_iter().flatten() {
                let data = state.event_sources.get_mut(&source).unwrap();
                if filter.read {
                    data.read.wake();
                }
                if filter.write {
                    data.write.wake();
                }
            }
        }

        // Clear expired timers from the timer queue
        state.timer_queue.clear_expired();
        // Clear notifier
        self.clear_notifications();
        Ok(())
    }

    pub(crate) fn register_timer(&self, expiry: Instant, waker: Waker) -> Id {
        self.state.borrow_mut().timer_queue.register(expiry, waker)
    }

    pub(crate) fn cancel_timer(&self, id: Id, expiry: Instant) {
        self.state.borrow_mut().timer_queue.cancel(id, expiry);
    }

    pub(crate) fn modify_timer(&self, id: Id, expiry: Instant, waker: &Waker) {
        self.state
            .borrow_mut()
            .timer_queue
            .modify(id, expiry, waker);
    }

    pub(crate) fn clear_notifications(&self) {
        if let Err(err) = self.notifier.clear() {
            log::error!(
                "{:?} Error clearing notifier: {err}",
                std::thread::current().id(),
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        let state = self.state.borrow();
        state.timer_queue.is_empty() && state.event_sources.is_empty()
    }
}

#[cfg(unix)]
type Poller = unix::Poller;
#[cfg(unix)]
pub(crate) type Notifier = WithFlag<unix::PollerNotifier>;

thread_local! {
    pub(crate) static REACTOR: Reactor<Poller> = Reactor::new().expect("Failed to initialize reactor");
}

// Can't put this in the generic impl block, otherwise we get privacy issues with the notifier type
impl Reactor<Poller> {
    /// Return a handle to a notifier object that can be used to wake up the reactor.
    pub(crate) fn notifier(&self) -> Arc<Notifier> {
        self.notifier.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::{array, sync::atomic::AtomicU32};

    use crate::test::MockWaker;

    use super::*;

    #[derive(Debug, Default)]
    struct MockNotifier(AtomicU32);

    impl EventNotifier for MockNotifier {
        fn clear(&self) -> io::Result<()> {
            self.0.store(0, Ordering::Relaxed);
            Ok(())
        }

        fn notify(&self) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct MockPoller {
        notifier: Arc<WithFlag<MockNotifier>>,
        registrations: BTreeMap<Source, Filter>,
        poll_input: Vec<(Source, Filter)>,
        poll_output: Vec<(Source, Filter)>,
        ret_error: bool,
    }

    impl MockPoller {
        fn ret(&self) -> io::Result<()> {
            (!self.ret_error)
                .then_some(())
                .ok_or_else(|| io::Error::other("test error"))
        }
    }

    impl EventPoller for MockPoller {
        type Notifier = MockNotifier;

        fn new() -> io::Result<(Self, Arc<WithFlag<Self::Notifier>>)>
        where
            Self: Sized,
        {
            let poller = MockPoller::default();
            let notif = poller.notifier.clone();
            Ok((poller, notif))
        }

        unsafe fn register(&mut self, source: Source) -> io::Result<()> {
            self.registrations.insert(source, Filter::default());
            self.ret()
        }

        fn modify(&mut self, source: Source, filter: Filter) -> io::Result<()> {
            *self.registrations.get_mut(&source).unwrap() = filter;
            self.ret()
        }

        fn deregister(&mut self, source: Source) -> io::Result<()> {
            self.registrations.remove(&source);
            self.ret()
        }

        fn poll(
            &mut self,
            _timeout: Option<Duration>,
            event_sources: impl Iterator<Item = (Source, Filter)>,
        ) -> io::Result<Option<impl Iterator<Item = (Source, Filter)> + '_>> {
            self.poll_input = event_sources.collect();
            let out = self.poll_output.clone().into_iter();
            self.ret().map(|_| Some(out))
        }
    }

    macro_rules! borrow {
        ($reactor:ident->$($tt:tt)*) => {
            $reactor.state.borrow_mut().$($tt)*
        };
    }

    #[test]
    fn flag_notifier() {
        let notifier = WithFlag::new(MockNotifier::default());

        // Send 10 notifications simultaneously
        std::thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| notifier.notify());
            }
        });

        assert!(notifier.is_notified.load(Ordering::Relaxed));
        // The inner notifier should have only been written once
        assert_eq!(notifier.inner.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn io_registration() {
        let reactor = Reactor::<MockPoller>::new().unwrap();
        assert_eq!(borrow!(reactor->event_sources.len()), 0);
        let waker1 = Arc::new(MockWaker::default());
        let waker2 = Arc::new(MockWaker::default());
        let waker3 = Arc::new(MockWaker::default());

        // Register
        unsafe { reactor.register_event(100).unwrap() };
        unsafe { reactor.register_event(101).unwrap() };
        assert_eq!(
            borrow!(reactor->poller.registrations.iter().collect::<Vec<_>>()),
            vec![(&100, &Filter::default()), (&101, &Filter::default())]
        );
        assert_eq!(
            borrow!(reactor->event_sources[&100].filter()),
            Filter::default()
        );
        assert_eq!(
            borrow!(reactor->event_sources[&101].filter()),
            Filter::default()
        );
        assert_eq!(borrow!(reactor->event_sources.len()), 2);

        // Enable event
        reactor
            .enable_event(100, Interest::Read, &waker1.clone().into())
            .unwrap();
        reactor
            .enable_event(101, Interest::Read, &waker2.clone().into())
            .unwrap();
        reactor
            .enable_event(101, Interest::Write, &waker3.clone().into())
            .unwrap();
        assert_eq!(
            borrow!(reactor->poller.registrations.iter().collect::<Vec<_>>()),
            vec![(&100, &Filter::read()), (&101, &Filter::both())]
        );
        assert_eq!(
            borrow!(reactor->event_sources[&100].filter()),
            Filter::read()
        );
        assert_eq!(
            borrow!(reactor->event_sources[&101].filter()),
            Filter::both()
        );
        assert_eq!(borrow!(reactor->event_sources.len()), 2);

        // Events not ready yet
        assert!(!reactor.is_event_ready(100, Interest::Read));
        assert!(!reactor.is_event_ready(101, Interest::Read));
        assert!(!reactor.is_event_ready(101, Interest::Write));

        // Wait
        borrow!(reactor->poller.poll_output = vec![(100, Filter::read()), (101, Filter::write())]);
        reactor.wait().unwrap();
        assert_eq!(
            borrow!(reactor->poller.poll_input),
            vec![(100, Filter::read()), (101, Filter::both())]
        );
        // Check events ready
        assert!(reactor.is_event_ready(100, Interest::Read));
        assert!(!reactor.is_event_ready(101, Interest::Read));
        assert!(reactor.is_event_ready(101, Interest::Write));
        assert!(waker1.get());
        assert!(!waker2.get());
        assert!(waker3.get());
        // Make sure fired events are no longer registered
        assert_eq!(
            borrow!(reactor->event_sources[&100].filter()),
            Filter::default()
        );
        assert_eq!(
            borrow!(reactor->event_sources[&101].filter()),
            Filter::read()
        );

        // Deregister
        reactor.deregister_event(101).unwrap();
        assert!(borrow!(reactor->poller.registrations.get(&100).is_some()));
        assert_eq!(borrow!(reactor->poller.registrations.len()), 1);
        assert!(borrow!(reactor->event_sources.get(&100).is_some()));
        assert_eq!(borrow!(reactor->event_sources.len()), 1);
    }

    #[test]
    fn empty_wait() {
        let reactor = Reactor::<MockPoller>::new().unwrap();
        reactor.wait().unwrap();
        assert!(borrow!(reactor->poller.poll_input.is_empty()));
        assert!(borrow!(reactor->poller.registrations.is_empty()));
        assert!(borrow!(reactor->event_sources.is_empty()));
    }

    #[test]
    fn poll_no_output() {
        let waker = Arc::new(MockWaker::default());
        let reactor = Reactor::<MockPoller>::new().unwrap();

        unsafe { reactor.register_event(100).unwrap() };
        unsafe { reactor.register_event(101).unwrap() };
        reactor
            .enable_event(100, Interest::Read, &waker.clone().into())
            .unwrap();
        reactor
            .enable_event(101, Interest::Read, &waker.clone().into())
            .unwrap();

        // Poll returns nothing, so no event should have fired
        reactor.wait().unwrap();
        assert!(!borrow!(reactor->poller.poll_input.is_empty()));
        assert!(!reactor.is_event_ready(100, Interest::Read));
        assert!(!reactor.is_event_ready(101, Interest::Read));
        assert!(!waker.get());
    }

    #[test]
    fn multiple_wakes() {
        let reactor = Reactor::<MockPoller>::new().unwrap();
        let events: [_; 5] = array::from_fn(|i| (i, Arc::new(MockWaker::default())));

        for (src, waker) in &events {
            unsafe { reactor.register_event(*src as Source).unwrap() };
            reactor
                .enable_event(*src as Source, Interest::Read, &waker.clone().into())
                .unwrap();
        }

        for i in [0, 1, 4] {
            borrow!(reactor->poller.poll_output = vec![(events[i].0 as Source, Filter::read())]);
            reactor.wait().unwrap();
            assert!(events[i].1.get());
        }
        assert!(!events[2].1.get());
        assert!(!events[3].1.get());
    }

    #[test]
    fn modify_registration() {
        let reactor = Reactor::<MockPoller>::new().unwrap();
        let wakers: [_; 3] = array::from_fn(|_| Arc::new(MockWaker::default()));
        unsafe { reactor.register_event(100).unwrap() };
        borrow!(reactor->poller.poll_output = vec![(100, Filter::read())]);

        reactor
            .enable_event(100, Interest::Read, &wakers[0].clone().into())
            .unwrap();
        reactor.wait().unwrap();
        assert!(wakers[0].get());

        reactor
            .enable_event(100, Interest::Read, &wakers[1].clone().into())
            .unwrap();
        reactor.wait().unwrap();
        assert!(wakers[1].get());

        reactor
            .enable_event(100, Interest::Read, &wakers[2].clone().into())
            .unwrap();
        reactor.wait().unwrap();
        assert!(wakers[2].get());

        // Make sure event is deleted properly after multiple enables
        reactor.deregister_event(100).unwrap();
        assert!(borrow!(reactor->poller.registrations.is_empty()));
        assert!(borrow!(reactor->event_sources.is_empty()));
    }

    #[test]
    fn disable_event_after_poll() {
        let reactor = Reactor::<MockPoller>::new().unwrap();
        let waker = Arc::new(MockWaker::default());

        unsafe { reactor.register_event(100).unwrap() };
        reactor
            .enable_event(100, Interest::Read, &waker.clone().into())
            .unwrap();
        reactor
            .enable_event(100, Interest::Write, &waker.clone().into())
            .unwrap();

        // Both events are polled, but only the read event fires
        borrow!(reactor->poller.poll_output = vec![(100, Filter::read())]);
        reactor.wait().unwrap();
        assert_eq!(
            borrow!(reactor->poller.poll_input),
            vec![(100, Filter::both())]
        );

        // Now only the write event is polled
        reactor.wait().unwrap();
        assert_eq!(
            borrow!(reactor->poller.poll_input),
            vec![(100, Filter::write())]
        );

        // Make sure event is deleted properly after multiple enables
        reactor.deregister_event(100).unwrap();
        assert!(borrow!(reactor->poller.registrations.is_empty()));
        assert!(borrow!(reactor->event_sources.is_empty()));
    }
}
