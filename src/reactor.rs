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

#[derive(Debug, Clone, Copy, PartialEq)]
struct Filter {
    read: bool,
    write: bool,
}

impl Filter {
    #[allow(unused)]
    fn read() -> Self {
        Self {
            read: true,
            write: false,
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
    pub(crate) fn deregister_event(&self, source: Source) {
        let mut state = self.state.borrow_mut();
        if state.event_sources.remove(&source).is_none() {
            log::error!(
                "{:?} Deregistering non-existent event source {source:?}",
                std::thread::current().id(),
            );
        }
        if let Err(err) = state.poller.deregister(source) {
            log::error!(
                "{:?} Error deregistering {source:?}: {err}",
                std::thread::current().id(),
            );
        }
    }

    /// Enable a registered event source.
    pub(crate) fn enable_event(&self, source: Source, interest: Interest, waker: &Waker) {
        let mut state = self.state.borrow_mut();
        if let Some(event_data) = state.event_sources.get_mut(&source) {
            let dir = match interest {
                Interest::Read => &mut event_data.read,
                Interest::Write => &mut event_data.write,
            };
            dir.enable(waker);
            let filter = event_data.filter();
            if let Err(err) = state.poller.modify(source, filter) {
                log::error!(
                    "{:?} Error enabling event source {source:?}: {err}",
                    std::thread::current().id()
                );
            }
        } else {
            log::error!(
                "{:?} Enabling non-existent event source {source:?}",
                std::thread::current().id()
            );
        }
    }

    /// Check if an event is ready since the last time `enable_event` was called
    pub(crate) fn is_event_ready(&self, source: Source, interest: Interest) -> bool {
        let state = self.state.borrow();
        if let Some(entry) = state.event_sources.get(&source) {
            let dir = match interest {
                Interest::Read => &entry.read,
                Interest::Write => &entry.write,
            };
            !dir.enabled
        } else {
            log::error!(
                "{:?} Checking non-existent event source {source:?}",
                std::thread::current().id(),
            );
            false
        }
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
