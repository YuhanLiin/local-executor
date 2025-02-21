mod unix;

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    task::Waker,
    time::{Duration, Instant},
};

use crate::{time::TimerQueue, Id};

#[cfg(unix)]
type Poller = unix::Poller;
#[cfg(unix)]
pub(crate) type Notifier = WithFlag<unix::PollerNotifier>;

/// Type of event that we're interested in receiving
#[derive(Debug, Clone, Copy)]
pub(crate) enum Interest {
    Read,
    Write,
}

#[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg(unix)]
pub(crate) struct EventHandle {
    source: Source,
    id: Id,
}

#[cfg(unix)]
type Source = std::os::fd::RawFd;
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
        event_sources: impl Iterator<Item = ((Source, Id), Filter)>,
    ) -> io::Result<Option<impl Iterator<Item = ((Source, Id), Filter)> + '_>>;
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

struct State {
    id: Id,
    poller: Poller,
    event_sources: BTreeMap<EventHandle, EventData>,
    timer_queue: TimerQueue,
}

pub(crate) struct Reactor {
    state: RefCell<State>,
    notifier: Arc<Notifier>,
}

/// General trait for the reactor used to wakeup futures
impl Reactor {
    /// Construct new reactor
    pub(crate) fn new() -> io::Result<Self> {
        let (poller, notifier) = Poller::new()?;
        Ok(Reactor {
            state: RefCell::new(State {
                id: const { Id::new(1) },
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
    pub(crate) unsafe fn register_event(&self, source: Source) -> io::Result<EventHandle> {
        let mut state = self.state.borrow_mut();
        let fd = source;
        state.poller.register(source)?;
        loop {
            let id = state.id;
            state.id = state.id.overflowing_incr();
            let handle = EventHandle { source: fd, id };
            // On the rare chance that the (ID, raw_fd) pair already exists, which can only happen
            // if the ID overflowed and the same FD is still registered on that ID, then just try
            // the next ID.
            match state
                .event_sources
                .insert(EventHandle { ..handle }, EventData::default())
            {
                None => break Ok(handle),
                // Restore the previous event source
                Some(prev_data) => {
                    state.event_sources.insert(handle, prev_data);
                }
            }
        }
    }

    /// Deregister event source from the reactor
    pub(crate) fn deregister_event(&self, handle: &EventHandle) {
        let mut state = self.state.borrow_mut();
        if state.event_sources.remove(handle).is_none() {
            log::error!(
                "{:?} Deregistering non-existent event source {handle:?}",
                std::thread::current().id(),
            );
        }
        if let Err(err) = state.poller.deregister(handle.source) {
            log::error!(
                "{:?} Error deregistering {handle:?}: {err}",
                std::thread::current().id(),
            );
        }
    }

    /// Enable a registered event source.
    pub(crate) fn enable_event(&self, handle: &EventHandle, interest: Interest, waker: &Waker) {
        let mut state = self.state.borrow_mut();
        if let Some(event_data) = state.event_sources.get_mut(handle) {
            let dir = match interest {
                Interest::Read => &mut event_data.read,
                Interest::Write => &mut event_data.write,
            };
            dir.enable(waker);
            let filter = event_data.filter();
            if let Err(err) = state.poller.modify(handle.source, filter) {
                log::error!(
                    "{:?} Error enabling event source {handle:?}: {err}",
                    std::thread::current().id()
                );
            }
        } else {
            log::error!(
                "{:?} Enabling non-existent event source {handle:?}",
                std::thread::current().id()
            );
        }
    }

    /// Check if an event is ready since the last time `enable_event` was called
    pub(crate) fn is_event_ready(&self, handle: &EventHandle, interest: Interest) -> bool {
        let state = self.state.borrow();
        if let Some(entry) = state.event_sources.get(handle) {
            let dir = match interest {
                Interest::Read => &entry.read,
                Interest::Write => &entry.write,
            };
            !dir.enabled
        } else {
            log::error!(
                "{:?} Checking non-existent event source {handle:?}",
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
            let event_sources = state
                .event_sources
                .iter()
                .map(|(h, d)| ((h.source, h.id), d.filter()));
            let revents = state.poller.poll(timeout, event_sources)?;
            // Now that we have awaken from the poll call, there's no need to send any
            // notifications to "wake up" from the poll, so we set the notified flag to prevent
            // our wakers from sending any notifications.
            self.notifier.set_to_notified();

            for ((source, id), filter) in revents.into_iter().flatten() {
                let handle = EventHandle { source, id };
                let data = state.event_sources.get_mut(&handle).unwrap();
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

    /// Return a handle to a notifier object that can be used to wake up the reactor.
    pub(crate) fn notifier(&self) -> Weak<Notifier> {
        Arc::downgrade(&self.notifier)
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

thread_local! {
    pub(crate) static REACTOR: Reactor = Reactor::new().expect("Failed to initialize reactor");
}
