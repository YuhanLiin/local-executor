#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{io, sync::Weak, task::Waker, time::Duration};

/// Type of events that we're interested in receiving
pub struct Interest {
    pub read: bool,
    pub write: bool,
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

/// General trait for the reactor used to wakeup futures
pub trait Reactor {
    type Notifier: Notifier + 'static;

    /// Construct new reactor
    fn new() -> io::Result<Self>
    where
        Self: Sized;

    /// Register new event source onto the reactor along with a Waker that will be pinged if an
    /// event is received on the source.
    ///
    /// SAFETY: The event source must not be dropped before its FD is cleared from the reactor via
    /// `wait()`.
    #[cfg(unix)]
    unsafe fn register<S: AsRawFd>(&self, source: &S, interest: Interest, waker: Waker);
    #[cfg(not(unix))]
    compile_error!("Unsupported operating system!");

    /// Wait for an event on the reactor with an optional timeout, then clears all event sources.
    fn wait(&self, timeout: Option<Duration>) -> io::Result<()>;

    /// Return a handle to a notifier object that can be used to wake up the reactor.
    fn notifier(&self) -> Weak<Self::Notifier>;
}

/// Object that wakes up the reactor
pub trait Notifier {
    fn notify(&self) -> io::Result<()>;
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub type ReactorImpl = unix::UnixReactor;

pub type NotifierImpl = <ReactorImpl as Reactor>::Notifier;
