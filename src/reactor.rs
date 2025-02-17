#[cfg(unix)]
use std::os::fd::{AsFd, RawFd};
use std::{io, sync::Weak, task::Waker, time::Duration};

use crate::Id;

/// Type of event that we're interested in receiving
#[derive(Debug, Clone, Copy)]
pub(crate) enum Interest {
    Read,
    Write,
}

#[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg(unix)]
pub(crate) struct EventHandle {
    fd: RawFd,
    id: Id,
}

/// General trait for the reactor used to wakeup futures
pub(crate) trait Reactor {
    type Notifier: Notifier + 'static;

    /// Construct new reactor
    fn new() -> io::Result<Self>
    where
        Self: Sized;

    #[cfg(unix)]
    /// Register new event source onto the reactor along with a Waker that will be pinged if an
    /// event is received on the source. Returns an unique ID to that event source.
    ///
    /// SAFETY: The event source must not be dropped before it's cleared from the reactor via
    /// `deregister()`.
    unsafe fn register<S: AsFd>(&self, source: &S) -> EventHandle;
    #[cfg(not(unix))]
    compile_error!("Unsupported operating system!");

    /// Deregister event source from the reactor
    fn deregister(&self, handle: &EventHandle);

    /// Enable a registered event source.
    fn enable_event(&self, handle: &EventHandle, interest: Interest, waker: &Waker);

    /// Wait for an event on the reactor with an optional timeout, then clears all event sources.
    fn wait(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// Return a handle to a notifier object that can be used to wake up the reactor.
    fn notifier(&self) -> Weak<Self::Notifier>;

    fn clear_notifications(&self);

    #[cfg(test)]
    fn is_empty(&self) -> bool;
}

/// Object that wakes up the reactor
pub(crate) trait Notifier {
    fn notify(&self) -> io::Result<()>;
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) type ReactorImpl = unix::UnixReactor;

pub(crate) type NotifierImpl = <ReactorImpl as Reactor>::Notifier;

thread_local! {
    pub(crate) static REACTOR: ReactorImpl = ReactorImpl::new().expect("Failed to initialize reactor");
}
