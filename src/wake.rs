#[derive(Clone)]
pub struct Wake(std::thread::Thread);

impl Wake {
    pub fn new() -> Self {
        Self(std::thread::current())
    }

    pub fn notify(&self) {
        self.0.unpark();
    }

    pub fn wait(&self, timeout: std::time::Duration) {
        std::thread::park_timeout(timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn notifications_before_wait_are_not_lost() {
        let wake = Wake::new();
        let started = Instant::now();
        wake.notify();
        wake.wait(Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
