#![allow(dead_code)]

use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
use std::thread;
use std::time::{Duration, SystemTime};

use crossbeam_channel as channel;
use parking_lot::RwLock;
use crate::debug::is_debug_mode;
use crate::manager::{StatusBehaviors, StatusBehaviorDefinitions};
use crate::model::*;
use crate::scheduler::ThreadPool;

const TIMEOUT: Duration = Duration::from_micros(16);
const LONG_TIMEOUT: Duration = Duration::from_micros(96);
const LOT_COUNTS: usize = 3;
const LONG_PARKING_ROUNDS: u8 = 16;
const SHORT_PARKING_ROUNDS: u8 = 4;

pub(crate) struct WorkerConfig {
    name: Option<String>,
    stack_size: usize,
    privileged: bool,
    max_idle: Arc<AtomicUsize>,
}

impl WorkerConfig {
    pub(crate) fn new(
        name: Option<String>,
        stack_size: usize,
        privileged: bool,
        max_idle: Arc<AtomicUsize>
    ) -> Self
    {
        WorkerConfig {
            name,
            stack_size,
            privileged,
            max_idle,
        }
    }

    pub(crate) fn stack_size(&mut self, size: usize) {
        self.stack_size = size;
    }

    pub(crate) fn privileged(&mut self, is_privileged: bool) {
        self.privileged = is_privileged;
    }

    pub(crate) fn name(&mut self, name: String) {
        if name.is_empty() {
            self.name = None;
        } else {
            self.name.replace(name);
        }
    }

    pub(crate) fn max_idle(&mut self, idle: Arc<AtomicUsize>) {
        self.max_idle = idle;
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self::new(
            None,
            0,
            false,
            Arc::new(AtomicUsize::new(0))
        )
    }
}

pub(crate) struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
    before_drop: Option<WorkerUpdate>,
    after_drop: Option<WorkerUpdate>,
}

struct Status(i8);

struct WorkCourier {
    target: Option<usize>,
    work: Option<Job>,
}

impl Worker {
    pub(crate) fn new(
        my_id: usize,
        pri_rx: channel::Receiver<Message>,
        rx: channel::Receiver<Message>,
        graveyard: Arc<RwLock<Vec<i8>>>,
        config: WorkerConfig,
        behavior_definition: &StatusBehaviors,
    ) -> Worker
    {
        behavior_definition.before_start(my_id);

        let thread: thread::JoinHandle<()> =
            Self::run(my_id, rx, pri_rx, graveyard, config);

        behavior_definition.after_start(my_id);

        Worker {
            id: my_id,
            thread: Some(thread),
            before_drop: behavior_definition.before_drop_clone(),
            after_drop: behavior_definition.after_drop_clone(),
        }
    }

    pub(crate) fn get_id(&self) -> usize {
        self.id
    }

    // Calling `retire` on a worker will block the thread until the worker has done its work, or wake
    // up from hibernation. This could block the caller for an undetermined amount of time.
    pub(crate) fn retire(&mut self) {
        if let Some(thread) = self.thread.take() {
            // make sure the work is done
            thread.join().unwrap_or_else(|err| {
                eprintln!("Unable to drop worker: {}, error: {:?}", self.id, err);
            });
        }
    }

    fn run(
        my_id: usize,
        rx: channel::Receiver<Message>,
        pri_rx: channel::Receiver<Message>,
        graveyard: Arc<RwLock<Vec<i8>>>,
        mut config: WorkerConfig,
    ) -> thread::JoinHandle<()>
    {
        let mut builder = thread::Builder::new();

        if config.name.is_some() {
            builder = builder.name(config.name.take().unwrap_or_default());
        }

        if config.stack_size > 0 {
            builder = builder.stack_size(config.stack_size);
        }

        let privileged = config.privileged;
        let max_idle: Arc<AtomicUsize> = config.max_idle;

        builder.spawn(move || {
            let mut courier = WorkCourier {
                target: None,
                work: None,
            };

            let mut since = if privileged {
                None
            } else {
                Some(SystemTime::now())
            };

            let mut idle: Option<Duration>;
            let mut pri_work_count: u8 = 0;

            // main worker loop
            loop {
                // get ready to take new work from the channel
                {
                    let g = graveyard.read();
                    if my_id >= g.len() {
                        // illegal case, always return
                        return;
                    }

                    if g[my_id] == -1 {
                        return;
                    }

                    if g[0] == -1
                        && (ThreadPool::is_forced_close() || pri_rx.is_empty() && rx.is_empty())
                    {
                        // if shutting down, check if we can abandon all work by checking forced
                        // close flag, or when all work have been processed.
                        return;
                    }
                }

                // wait for work loop
                if let Status(-1) =
                    Worker::check_queues(my_id, &pri_rx, &rx, &mut pri_work_count, &mut courier)
                {
                    // if the channels are disconnected, return
                    return;
                }

                // if there's a job, get it done first, and calc the idle period since last actual job
                idle = if courier.work.is_some() {
                    Worker::handle_work(courier.work.take(), &mut since)
                } else if since.is_some() {
                    Worker::calc_idle(&since)
                } else {
                    None
                };

                //===========================
                //    after-work handling
                //===========================

                // if it's a target kill, handle it now
                if let Some(id) = courier.target.take() {
                    let mut g = graveyard.write();

                    // otherwise, update the graveyard
                    if id < g.len() {
                        (*g)[id] = -1;
                    }

                    if (id == 0 && ThreadPool::is_forced_close()) || id == my_id {
                        // if my id or a forced kill, just quit
                        return;
                    }
                }

                // if idled longer than the expected worker life for unprivileged workers,
                // then we're done now -- self-purging.
                if let Some(idle) = idle.take() {
                    let max = max_idle.load(Ordering::Relaxed) as u128;
                    if max.gt(&0) && max.le(&idle.as_millis()) {
                        return;
                    }
                }
            }
        }).unwrap()
    }

    fn check_queues(
        id: usize,
        pri_chan: &channel::Receiver<Message>,
        norm_chan: &channel::Receiver<Message>,
        pri_work_count: &mut u8,
        courier: &mut WorkCourier,
    ) -> Status
    {
        // wait for work loop, 1/3 of workers will long-park for priority work, and 1/3 of workers
        // will long-park for normal work, the remainder 1/3 workers will be fluid and constantly
        // query both queues -- whichever yield a task, then it will execute that task.
        if *pri_work_count < 255 {
            // 1/3 of the workers is designated to wait longer for prioritised jobs
            let parking = id % LOT_COUNTS == 0;
            match Worker::fetch_work(
                pri_chan, norm_chan.is_empty(), !parking
            ) {
                Ok(message) => {
                    // message is the only place that can update the "done" field
                    Worker::unpack_message(message, courier);

                    if *pri_work_count < 4 {
                        // only add if we're below the continuous pri-work cap
                        *pri_work_count += 1;
                    } else if norm_chan.is_full() {
                        // if we've done 4 or more priority work in a row, check if
                        // we should skip if the normal channel is full and maybe
                        // blocking, by setting the special number
                        *pri_work_count = 255;
                    }

                    return Status(0);
                },
                Err(channel::RecvTimeoutError::Disconnected) => {
                    // sender has been dropped
                    return Status(-1);
                },
                Err(channel::RecvTimeoutError::Timeout) => {
                    // if chan empty, do nothing and fall through to the normal chan handle
                    // fall-through
                }
            };
        } else {
            // if the worker has performed 4 consecutive prioritized work and the normal
            // channel is full, we skip the priority work once to pick up a normal work
            // such that it won't be blocked forever; meanwhile, reset the counter.
            *pri_work_count = 0;
        }

        // 1/3 of the workers is designated to wait longer for normal jobs
        match Worker::fetch_work(
            norm_chan, pri_chan.is_empty(), id % LOT_COUNTS == 1
        ) {
            Ok(message) => {
                // message is the only place that can update the "done" field
                Worker::unpack_message(message, courier);
                *pri_work_count = 0;

                return Status(0);
            },
            Err(channel::RecvTimeoutError::Disconnected) => {
                // sender has been dropped
                return Status(-1);
            },
            Err(channel::RecvTimeoutError::Timeout) => {
                // nothing to receive yet
            }
        };

        Status(0)
    }

    fn fetch_work(
        main_chan: &channel::Receiver<Message>,
        side_chan_empty: bool,
        can_skip: bool
    ) -> Result<Message, channel::RecvTimeoutError>
    {
        let mut wait = 0;
        let rounds = if can_skip {
            SHORT_PARKING_ROUNDS
        } else {
            LONG_PARKING_ROUNDS
        };

        loop {
            wait += 1;

            match main_chan.try_recv() {
                Ok(work) => return Ok(work),
                Err(channel::TryRecvError::Disconnected) => return Err(channel::RecvTimeoutError::Disconnected),
                Err(channel::TryRecvError::Empty) => {
                    if can_skip && !side_chan_empty {
                        // if there're normal work in queue, break to fetch the normal work
                        return Err(channel::RecvTimeoutError::Timeout);
                    }
                },
            }

            if wait > rounds {
                return Err(channel::RecvTimeoutError::Timeout);
            }
        }
    }

    fn handle_work(work: Option<Job>, since: &mut Option<SystemTime>) -> Option<Duration> {
        let mut idle = None;
        if let Some(work) = work {
            work.call_box();

            if since.is_some() {
                idle = Worker::calc_idle(&since);
                *since = Some(SystemTime::now());
            }
        }

        idle
    }

    fn unpack_message(
        message: Message,
        courier: &mut WorkCourier
    ) {
        match message {
            Message::NewJob(job) => {
                courier.work = Some(job);
            },
            Message::Terminate(target) => {
                courier.target = Some(target);
            }
        }
    }

    fn calc_idle(since: &Option<SystemTime>) -> Option<Duration> {
        if let Some(s) = since {
            if let Ok(e) = s.elapsed() {
                return Some(e);
            }
        }

        None
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(behavior) = self.before_drop {
            behavior(self.id);
        }

        if is_debug_mode() {
            println!("Dropping worker {}", self.id);
        }

        self.retire();

        if let Some(behavior) = self.after_drop {
            behavior(self.id);
        }
    }
}