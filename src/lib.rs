mod reactor;

use std::{
    cell::LazyCell,
    future::Future,
    pin::pin,
    sync::Weak,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use reactor::{Notifier, NotifierImpl, Reactor, ReactorImpl};

thread_local! {
    static REACTOR: LazyCell<ReactorImpl> = LazyCell::new(|| ReactorImpl::new().expect("Failed to initialize reactor"));
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
