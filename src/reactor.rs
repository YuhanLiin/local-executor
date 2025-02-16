#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{io, sync::Weak, task::Waker, time::Duration};

use crate::Id;

/// Type of event that we're interested in receiving
#[derive(Debug, Clone, Copy)]
pub(crate) enum Interest {
    Read,
    Write,
}

impl Interest {
    pub(crate) fn read() -> Self {
        Self::Read
    }

    pub(crate) fn write() -> Self {
        Self::Write
    }
}

pub(crate) enum WakeMode<'a> {
    Enable(&'a Waker),
    Disable,
}

impl<'a> From<&'a Waker> for WakeMode<'a> {
    fn from(waker: &'a Waker) -> Self {
        Self::Enable(waker)
    }
}

#[derive(Debug)]
#[cfg(unix)]
pub(crate) struct Source(RawFd);
#[cfg(unix)]
impl<'a> From<BorrowedFd<'a>> for Source {
    fn from(fd: BorrowedFd<'a>) -> Self {
        Self(fd.as_raw_fd())
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
    unsafe fn register(&self, source: &Source, interest: Interest, waker: Waker) -> Id;

    /// Deregister event source from the reactor
    fn deregister(&self, id: Id, source: &Source);

    /// Change the waker associated with a registered event source.
    ///
    /// If waker is not supplied, then this event is disabled.
    fn modify(&self, id: Id, source: &Source, mode: WakeMode);

    #[cfg(not(unix))]
    compile_error!("Unsupported operating system!");

    /// Wait for an event on the reactor with an optional timeout, then clears all event sources.
    fn wait(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// Return a handle to a notifier object that can be used to wake up the reactor.
    fn notifier(&self) -> Weak<Self::Notifier>;

    fn clear(&self);
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
