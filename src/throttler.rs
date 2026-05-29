use std::time::{Duration, Instant};

pub struct Throttler {
    last_update: Instant,
    min_interval: Duration,
}

impl Throttler {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            last_update: Instant::now(),
            min_interval,
        }
    }

    pub async fn wait(&mut self) {
        if self.min_interval.is_zero() {
            return;
        }
        let now = Instant::now();
        let next_allowed = self.last_update + self.min_interval;
        if next_allowed > now {
            tokio::time::sleep(next_allowed - now).await;
            self.last_update = next_allowed;
        } else {
            self.last_update = now;
        }
    }
}
