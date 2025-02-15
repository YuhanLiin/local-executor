use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Wake,
};

#[derive(Debug, Default)]
pub struct MockWaker(pub AtomicBool);

impl Wake for MockWaker {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl MockWaker {
    pub fn set(&self, b: bool) {
        self.0.store(b, Ordering::Relaxed);
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
