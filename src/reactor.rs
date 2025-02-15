#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{io, sync::Weak, task::Waker, time::Duration};

use crate::Id;

/// Type of events that we're interested in receiving
#[derive(Debug, Clone, Copy)]
pub(crate) struct Interest {
    pub(crate) read: bool,
    pub(crate) write: bool,
}

impl Interest {
    pub(crate) fn read() -> Self {
        Self {
            read: true,
            write: false,
        }
    }

    pub(crate) fn write() -> Self {
        Self {
            read: false,
            write: true,
        }
    }

    pub(crate) fn both() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}

/// General trait for the reactor used to wakeup futures
pub(crate) trait Reactor {
    type Notifier: Notifier + 'static;

    /// Construct new reactor
    fn new() -> io::Result<Self>
    where
        Self: Sized;

    /// Register new event source onto the reactor along with a Waker that will be pinged if an
    /// event is received on the source. Returns an unique ID to that event source.
    ///
    /// SAFETY: The event source must not be dropped before it's cleared from the reactor via
    /// `deregister()`.
    #[cfg(unix)]
    unsafe fn register<S: AsRawFd>(&self, source: &S, interest: Interest, waker: Waker) -> Id;

    /// Deregister event source from the reactor
    #[cfg(unix)]
    fn deregister<S: AsRawFd>(&self, id: Id, source: &S);

    #[cfg(unix)]
    /// Change the interested events and waker associated with a registered event source.
    fn modify<S: AsRawFd>(&self, id: Id, source: &S, interest: Interest, waker: &Waker);

    #[cfg(not(unix))]
    compile_error!("Unsupported operating system!");

    /// Wait for an event on the reactor with an optional timeout, then clears all event sources.
    fn wait(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// Return a handle to a notifier object that can be used to wake up the reactor.
    fn notifier(&self) -> Weak<Self::Notifier>;
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
