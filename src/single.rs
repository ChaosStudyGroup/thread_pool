#![allow(dead_code)]

use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Once, ONCE_INIT};
use crate::config::{Config, ConfigStatus};
use crate::debug::is_debug_mode;
use crate::scheduler::{PoolManager, ThreadPool};

static ONCE: Once = ONCE_INIT;
static mut POOL: Option<Pool> = None;

struct Pool {
    store: ThreadPool,
    closing: bool,
    auto_mode: bool,
    auto_adjust_handler: Option<JoinHandle<()>>,
}

impl Pool {
    #[inline]
    fn inner() -> Option<&'static mut Pool> {
        unsafe { POOL.as_mut() }
    }

    #[inline]
    fn is_some() -> bool {
        unsafe { POOL.is_some() }
    }

    #[inline]
    fn toggle_auto_mode(&mut self, enabled: bool) {
        if self.auto_mode == enabled {
            return;
        }

        self.auto_mode = enabled;
        self.store.toggle_auto_scale(enabled);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if !self.closing {
            close();
        }
    }
}

#[inline]
pub fn initialize(size: usize) {
    init_with_config(size, Config::default());
}

pub fn initialize_with_auto_adjustment(size: usize, period: Option<Duration>) {
    let mut config = Config::default();
    config.set_refresh_period(period);
    init_with_config(size, config);
}

pub fn init_with_config(size: usize, config: Config) {
    assert!(!Pool::is_some(), "You are trying to initialize the thread pools multiple times!");

    let pool_size = match size {
        0 => 1,
        _ => size,
    };

    ONCE.call_once(|| {
        create(pool_size, config);
    });
}

pub fn run<F: FnOnce() + Send + 'static>(f: F) {
    match Pool::inner() {
        Some(pool) => {
            // if pool has been created, execute in proper mode.
            if pool.closing && is_debug_mode() {
                eprintln!("Trying to run jobs when the pool is closing...");
                return;
            }

            if pool.store.exec(f, true).is_err() && is_debug_mode() {
                eprintln!("The execution of this job has failed...");
            }

            return;
        },
        None => {
            // This could happen after the pool is closed, just execute the job
            thread::spawn(f);

            if is_debug_mode() {
                eprintln!("The pool has been poisoned... The thread pool should be restarted...");
            }
        },
    };
}

pub fn close() {
    if let Some(mut pool) = unsafe { POOL.take() } {
        pool.closing = true;
        pool.store.close();
    }
}

pub fn force_close() {
    if let Some(mut pool) = unsafe { POOL.take() } {
        pool.closing = true;
        pool.store.force_close();
    }
}

pub fn resize(size: usize) -> JoinHandle<()> {
    thread::spawn(move || {
        if size == 0 {
            close();
        }

        if let Some(pool) = Pool::inner() {
            pool.store.resize(size);
            return;
        }

        create(size, Config::default());
    })
}

pub fn update_auto_adjustment_mode(enabled: bool) {
    if let Some(pool) = Pool::inner() {
        if pool.auto_mode == enabled {
            return;
        }

        if !enabled && pool.auto_adjust_handler.is_some() {
            stop_auto_adjustment(pool);
        }

        pool.toggle_auto_mode(enabled);
    }
}

pub fn reset_auto_adjustment_period(period: Option<Duration>) {
    if let Some(pool) = Pool::inner() {
        // stop the previous auto job regardless
        stop_auto_adjustment(pool);

        // initiate the new auto adjustment job if configured
        if let Some(actual_period) = period {
            pool.toggle_auto_mode(true);
            pool.auto_adjust_handler = Some(start_auto_adjustment(actual_period));
        }
    }
}

fn trigger_auto_adjustment() {
    if let Some(pool) = Pool::inner() {
        pool.store.auto_adjust();
    }
}

fn start_auto_adjustment(period: Duration) -> JoinHandle<()> {
    let one_second = Duration::from_secs(1);
    let actual_period = if period < one_second {
        one_second
    } else {
        period
    };

    thread::spawn(move || {
        thread::sleep(actual_period);

        loop {
            trigger_auto_adjustment();
            thread::sleep(actual_period);
        }
    })
}

fn stop_auto_adjustment(pool: &mut Pool) {
    if let Some(handler) = pool.auto_adjust_handler.take() {
        handler.join().unwrap_or_else(|e| {
            eprintln!("Unable to join the thread: {:?}", e);
        });

        if pool.auto_mode {
            pool.toggle_auto_mode(false);
        }
    }
}

fn create(size: usize, config: Config) {
    if size == 0 {
        return;
    }

    let (auto_mode, handler) =
        if let Some(period) = config.refresh_period() {
            (true, Some(start_auto_adjustment(period)))
        } else {
            (false, None)
        };

    // Make the pool
    let mut store = ThreadPool::new_with_config(size, config);
    store.toggle_auto_scale(auto_mode);

    // Put it in the heap so it can outlive this call
    unsafe {
        POOL.replace(Pool {
            store,
            closing: false,
            auto_mode,
            auto_adjust_handler: handler,
        });
    }
}
