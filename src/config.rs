use std::time::Duration;
use crate::manager::{StatusBehaviors, StatusBehaviorSetter};
use crate::model::WorkerUpdate;

#[derive(Clone)]
pub struct Config {
    pool_name: Option<String>,
    worker_behaviors: StatusBehaviors,
    refresh_period: Option<Duration>,
}

impl Config {
    pub fn new() -> Self {
        Config {
            pool_name: None,
            worker_behaviors: StatusBehaviors::default(),
            refresh_period: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

pub trait ConfigStatus {
    fn pool_name(&self) -> Option<String>;
    fn refresh_period(&self) -> Option<Duration>;
    fn worker_behavior(&self) -> StatusBehaviors;
    fn set_pool_name(&mut self, name: String);
    fn set_refresh_period(&mut self, period: Option<Duration>);
    fn set_worker_behavior(&mut self, behavior: StatusBehaviors);
}

impl ConfigStatus for Config {
    fn pool_name(&self) -> Option<String> {
        self.pool_name.clone()
    }

    fn refresh_period(&self) -> Option<Duration> {
        self.refresh_period
    }

    fn worker_behavior(&self) -> StatusBehaviors {
        self.worker_behaviors.clone()
    }

    fn set_pool_name(&mut self, name: String) {
        if name.is_empty() {
            self.pool_name = None;
        } else {
            self.pool_name.replace(name);
        }
    }

    fn set_refresh_period(&mut self, period: Option<Duration>) {
        self.refresh_period = period;
    }

    fn set_worker_behavior(&mut self, behavior: StatusBehaviors) {
        self.worker_behaviors = behavior
    }
}

impl StatusBehaviorSetter for Config {
    fn set_before_start(&mut self, behavior: WorkerUpdate) {
        self.worker_behaviors.set_before_start(behavior);
    }

    fn set_after_start(&mut self, behavior: WorkerUpdate) {
        self.worker_behaviors.set_after_start(behavior);
    }

    fn set_before_drop(&mut self, behavior: WorkerUpdate) {
        self.worker_behaviors.set_before_drop(behavior);
    }

    fn set_after_drop(&mut self, behavior: WorkerUpdate) {
        self.worker_behaviors.set_after_drop(behavior);
    }
}