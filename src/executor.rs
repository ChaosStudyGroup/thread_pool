#![allow(unused)]

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, mpsc::RecvTimeoutError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use std::thread::{self, Thread, JoinHandle};

use crate::ExecutionError;
use async_task::{Task};
use crossbeam_deque::Worker;
use crossbeam_channel as channel;
use crossbeam_utils::sync::Parker;
use crossbeam_channel::{Sender, Receiver};

#[macro_export]
macro_rules! pin_mut {
    ( $( $x:ident ), *) => {
        $(
            // Move the value to ensure that it is owned
            let mut $x = $x;

            // Shadow the original binding so that it can't be directly accessed
            // ever again.
            #[allow(unused_mut)]
            let mut $x = unsafe {
                $crate::core_export::pin::Pin::new_unchecked(&mut $x)
            };
        ) *
    }
}

pub fn block_on<T>(mut fut: impl Future<Output=T>) -> Result<T, ExecutionError> {
    thread_local! {
        static CACHE: RefCell<(Parker, Waker)> = {
            let parker = Parker::new();
            let unparker = parker.unparker().clone();
            let waker = async_task::waker_fn(move || unparker.unpark());

            RefCell::new((parker, waker))
        };
    }

    CACHE.with(|cache| {
        let (parker, waker) =
            &mut *cache.try_borrow_mut().expect("recursive entry forbidden ... ");

        pin_mut!(fut);

        loop {
            match fut.as_mut().poll(&mut Context::from_waker(&waker)) {
                Poll::Ready(val) => return Ok(val),
                Poll::Pending => parker.park(),
            }
        }
    })
}

pub struct FutPool {
    workers: Vec<Thread>,
}

impl FutPool {
    pub fn spawn<F, R>(fut: F) -> Receiver<R>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx): (Sender<R>, Receiver<R>) = channel::bounded(1);

        let f = Box::pin(async move {
            tx.send(fut.await).expect("failed to send the result back ... ")
        });

        //TODO: send the task to the pool of workers

        rx
    }
}

pub fn spawn<F, R>(fut: F) -> Receiver<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx): (Sender<R>, Receiver<R>) = channel::bounded(1);

    let f = Box::pin(async move {
        tx.send(fut.await).expect("failed to send the result back ... ")
    });

    rx
}

thread_local! {
    static QUEUE: Arc<Worker<Task<()>>> = Arc::new(Worker::new_fifo());
}

pub(crate) fn enqueue<F, R>(future: F) -> channel::Receiver<R>
where
    F: Future<Output = R> + 'static,
    R: Send + 'static,
{
    // If the task gets woken up, it will be sent into this channel.
    let (s, r) = channel::bounded(1);
    let fut = async move {
        s.send(future.await);
    };

    let schedule = move |task| {
        QUEUE.with(|q| {
            q.push(task);
        });
    };

    // Create a task with the future and the schedule function.
    let (task, handle) =
        async_task::spawn_local(fut, schedule, ());

    task.run();

    r
}

fn poll() {
    QUEUE.with(|q| {
        if let Some(mut task) = q.pop() {
            task.run();
        }
    });
}
