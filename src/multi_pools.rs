#![allow(dead_code)]

use crate::scheduler::{PoolManager, PoolState, ThreadPool};
use crate::debug::is_debug_mode;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::sync::{Once, ONCE_INIT};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

static ONCE: Once = ONCE_INIT;
static mut MULTI_POOL: Option<PoolStore> = None;

struct PoolStore {
    store: HashMap<String, Box<ThreadPool>>,
    auto_adjust_period: Option<Duration>,
    auto_adjust_handler: Option<JoinHandle<()>>,
    auto_adjust_register: HashSet<String>,
}

impl PoolStore {
    #[inline]
    fn inner() -> Option<&'static mut PoolStore> {
        unsafe { MULTI_POOL.as_mut() }
    }

    #[inline]
    fn is_some() -> bool {
        unsafe { MULTI_POOL.is_some() }
    }
}

#[inline]
pub fn initialize(keys: HashMap<String, usize>) {
    initialize_with_auto_adjustment(keys, None);
}

pub fn initialize_with_auto_adjustment(keys: HashMap<String, usize>, period: Option<Duration>) {
    if keys.is_empty() {
        return;
    }

    assert!(!PoolStore::is_some(), "You are trying to initialize the thread pools multiple times!");

    ONCE.call_once(|| {
        create(keys, period);
    });
}

pub fn run_with<F: FnOnce() + Send + 'static>(key: String, f: F) {
    match PoolStore::inner() {
        Some(pool) => {
            // if pool has been created
            if let Some(pool_info) = pool.store.get_mut(&key) {
                if pool_info.exec(f, false).is_err() && is_debug_mode() {
                    eprintln!("The execution of this job has failed...");
                }

                return;
            }
        },
        None => {
            // meanwhile, lazy initialize (again?) the pool
            let mut base_pool = HashMap::with_capacity(1);
            base_pool.insert(key, 1);

            initialize(base_pool);

            // otherwise, spawn to a new thread for the work;
            thread::spawn(f);

            if is_debug_mode() {
                eprintln!("The pool has been poisoned... The thread pool should be restarted...");
            }
        }
    };
}

pub fn close() {
    unsafe {
        if let Some(pool) = MULTI_POOL.take() {
            for (_, mut pool_info) in pool.store {
                pool_info.clear();
            }
        }
    }
}

pub fn resize_pool(pool_key: String, size: usize) {
    if pool_key.is_empty() {
        return;
    }

    thread::spawn(move || {
        if let Some(pools) = PoolStore::inner() {
            if let Some(pool_info) = pools.store.get_mut(&pool_key) {
                pool_info.resize(size);
            }
        }
    });
}

pub fn remove_pool(key: String) -> Option<JoinHandle<()>> {
    if key.is_empty() {
        return None;
    }

    //TODO: remove from the auto_adjust_handlers as well...

    let handler = thread::spawn(move || {
        if let Some(pools) = PoolStore::inner() {
            if let Some(mut pool_info) = pools.store.remove(&key) {
                pool_info.clear();
            }
        }
    });

    Some(handler)
}

pub fn add_pool(key: String, size: usize) -> Option<JoinHandle<()>> {
    if key.is_empty() || size == 0 {
        return None;
    }

    let handler = thread::spawn(move || {
        if let Some(pools) = PoolStore::inner() {
            if let Some(pool_info) = pools.store.get_mut(&key) {
                if pool_info.get_size() != size {
                    pool_info.resize(size);
                    return;
                }
            }

            pools.store.insert(key, Box::new(ThreadPool::new(size)));
        }
    });

    Some(handler)
}

fn create(keys: HashMap<String, usize>, period: Option<Duration>) {
    let size = keys.len();
    let mut store = HashMap::with_capacity(size);

    for (key, size) in keys {
        if key.is_empty() || size == 0 {
            continue;
        }

        store.entry(key).or_insert(Box::new(ThreadPool::new(size)));
    }

    // Make the pool
    let pool = Some(PoolStore {
        store,
        auto_adjust_period: period,
        auto_adjust_handler: None,
        auto_adjust_register: HashSet::with_capacity(size),
    });

    unsafe {
        // Put it in the heap so it can outlive this call
        MULTI_POOL = mem::transmute(pool);
    }
}

pub fn start_auto_adjustment(period: Duration) {
    if let Some(pools) = PoolStore::inner() {
        if pools.auto_adjust_register.is_empty() {
            return;
        }

        if pools.auto_adjust_handler.is_some() {
            stop_auto_adjustment();
        }

        let five_second = Duration::from_secs(5);
        let actual_period = if period < five_second {
            five_second
        } else {
            period
        };

        pools.auto_adjust_period = Some(actual_period.clone());
        pools.auto_adjust_handler = Some(thread::spawn(move || {
            thread::sleep(actual_period);

            loop {
                trigger_auto_adjustment();
                thread::sleep(actual_period);
            }
        }));
    }
}

pub fn stop_auto_adjustment() {
    if let Some(pools) = PoolStore::inner() {
        if let Some(handler) = pools.auto_adjust_handler.take() {
            handler.join().unwrap_or_else(|e| {
                eprintln!("Unable to join the thread: {:?}", e);
            });
        }

        if !pools.auto_adjust_register.is_empty() {
            pools.auto_adjust_register = HashSet::with_capacity(pools.store.len());
        }

        pools.auto_adjust_period = None;
    }
}

pub fn reset_auto_adjustment_period(period: Option<Duration>) {
    // stop the previous auto job regardless
    stop_auto_adjustment();

    // initiate the new auto adjustment job if configured
    if let Some(actual_period) = period {
        start_auto_adjustment(actual_period);
    }
}

pub fn toggle_pool_auto_mode(key: String, auto_adjust: bool) {
    if let Some(pool) = PoolStore::inner() {
        if !pool.store.contains_key(&key) {
            return;
        }

        if pool.auto_adjust_register.is_empty() && !auto_adjust {
            return;
        }

        if let Some(pool_info) = pool.store.get_mut(&key) {
            pool_info.toggle_auto_scale(auto_adjust);
        }

        match auto_adjust {
            true => {
                let to_launch_handler = pool.auto_adjust_register.is_empty();

                pool.auto_adjust_register.insert(key);

                if to_launch_handler {
                    if let Some(period) = pool.auto_adjust_period {
                        start_auto_adjustment(period);
                    } else {
                        start_auto_adjustment(Duration::from_secs(10));
                    }
                }
            },
            false => {
                pool.auto_adjust_register.remove(&key);
                if pool.auto_adjust_register.is_empty() {
                    stop_auto_adjustment();
                }
            },
        };
    }
}

pub fn is_pool_in_auto_mode(key: String) -> bool {
    if let Some(pool) = PoolStore::inner() {
        return pool.auto_adjust_register.contains(&key);
    }

    false
}

fn trigger_auto_adjustment() {
    if let Some(pools) = PoolStore::inner() {
        if pools.auto_adjust_register.is_empty() {
            return;
        }

        for key in pools.auto_adjust_register.iter() {
            if let Some(pool_info) = pools.store.get_mut(key) {
                pool_info.auto_adjust();
            }
        }
    }
}
