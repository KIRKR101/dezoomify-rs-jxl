use std::time::{Duration, Instant};

pub struct Throttler {
    last_update: Option<Instant>,
    min_interval: Duration,
}

impl Throttler {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            last_update: None,
            min_interval,
        }
    }

    pub async fn wait(&mut self) {
        if self.min_interval.is_zero() {
            return;
        }
        let now = Instant::now();
        if let Some(last_update) = self.last_update {
            let next_allowed = last_update + self.min_interval;
            if next_allowed > now {
                tokio::time::sleep(next_allowed - now).await;
                self.last_update = Some(next_allowed);
                return;
            }
        }
        self.last_update = Some(now);
    }
}
