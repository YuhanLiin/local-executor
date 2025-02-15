mod reactor;
#[cfg(test)]
mod test;
pub mod timer;

use std::{
    future::Future,
    num::NonZero,
    pin::pin,
    sync::Weak,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use reactor::{Notifier, NotifierImpl, Reactor, REACTOR};

// Option<Id> will be same size as `usize`
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq, Hash)]
struct Id(NonZero<usize>);

impl Id {
    const fn new(n: usize) -> Self {
        Id(NonZero::new(n).expect("expected non-zero ID"))
    }

    const fn overflowing_incr(&self) -> Self {
        // Wrap back around to 1 on overflow
        match self.0.checked_add(1) {
            Some(next) => Self(next),
            None => const { Id::new(1) },
        }
    }
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, wake, wake_by_ref, waker_drop);

// Use a weak pointer to the reactor's notifier as the waker, which will wake up the reactor when
// it's waiting.
fn create_waker(notifier: Weak<NotifierImpl>) -> Waker {
    let raw = RawWaker::new(notifier.into_raw() as *const (), &WAKER_VTABLE);
    // SAFETY: WAKER_VTABLE follows all safety guarantees
    unsafe { Waker::from_raw(raw) }
}

unsafe fn waker_clone(ptr: *const ()) -> RawWaker {
    let weak = Weak::from_raw(ptr as *const NotifierImpl);
    let clone = Weak::clone(&weak);
    std::mem::forget(weak);
    RawWaker::new(clone.into_raw() as *const (), &WAKER_VTABLE)
}

unsafe fn waker_drop(ptr: *const ()) {
    drop(Weak::from_raw(ptr as *const NotifierImpl));
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let weak = Weak::from_raw(ptr as *const NotifierImpl);
    if let Some(arc) = weak.upgrade() {
        let _ = arc.notify();
    }
    std::mem::forget(weak);
}

unsafe fn wake(ptr: *const ()) {
    wake_by_ref(ptr);
    waker_drop(ptr);
}

pub fn block_on<T, F>(mut fut: F) -> T
where
    F: Future<Output = T>,
{
    let mut fut = pin!(fut);
    let waker = create_waker(REACTOR.with(|r| r.notifier()));
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(&waker)) {
            return out;
        }

        // TODO use proper timeouts
        REACTOR.with(|r| r.wait(None)).expect("Reactor wait failed");
    }
}
