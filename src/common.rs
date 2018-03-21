use std::thread;
use std::sync::{Arc, mpsc, Mutex};

type Job = Box<FnBox + Send + 'static>;

trait FnBox {
    fn call_box(self: Box<Self>);
}

impl<F: FnOnce()> FnBox for F {
    fn call_box(self: Box<F>) {
        (*self)()
    }
}

enum Message {
    NewJob(Job),
    Terminate(usize),
}

pub struct ThreadPool {
    workers: Vec<Worker>,
    last_id: usize,
    sender: mpsc::Sender<Message>,
    receiver: Arc<Mutex<mpsc::Receiver<Message>>>,
}

impl ThreadPool {
    pub fn new(size: usize) -> ThreadPool {
        let pool_size = match size {
            _ if size < 1 => 1,
            _ => size,
        };

        let (sender, receiver) = mpsc::channel();
        let receiver = Arc::new(Mutex::new(receiver));
        let start = 1;

        let mut workers = Vec::with_capacity(pool_size);
        for id in 0..pool_size {
            workers.push(Worker::new((start + id), Arc::clone(&receiver)));
        }

        ThreadPool {
            workers,
            last_id: start + pool_size - 1,
            sender,
            receiver,
        }
    }

    pub fn execute<F>(&self, f: F) where F: FnOnce() + Send + 'static {
        let job = Box::new(f);
        self.sender.send(Message::NewJob(job)).unwrap_or_else(|err| {
            println!("Unable to distribute the job: {}", err);
        });
    }

    pub fn extend(&mut self, size: usize) {
        if size == 0 { return; }

        // the start id is the next integer from the last worker's id
        let start = self.last_id + 1;

        for id in 0..size {
            let worker = Worker::new((start + id), Arc::clone(&self.receiver));
            self.workers.push(worker);
        }

        self.last_id += size;
    }

    pub fn kill_worker(&mut self, id: usize) {
        let mut index = 0;

        while index < self.workers.len() {
            if self.workers[index].id == id {
                let mut worker = self.workers.swap_remove(index);

                self.sender.send(Message::Terminate(id)).unwrap_or_else(|err| {
                    println!("Unable to send message: {}", err);
                });

                if let Some(thread) = worker.thread.take() {
                    thread.join().expect("Couldn't join on the associated thread");
                }

                return;
            }

            index += 1;
        }
    }

    pub fn clear(&mut self) {
        for _ in &mut self.workers {
            self.sender.send(Message::Terminate(0)).unwrap_or_else(|err| {
                println!("Unable to send message: {}", err);
            });
        }

        for worker in &mut self.workers {
            if let Some(thread) = worker.thread.take() {
                thread.join().expect("Couldn't join on the associated thread");
            }
        }
    }
}

pub trait PoolState {
    fn get_first_worker_id(&self) -> Option<usize>;
    fn get_last_worker_id(&self) -> Option<usize>;
    fn get_next_worker_id(&self, id: usize) -> Option<usize>;
}

impl PoolState for ThreadPool {
    fn get_first_worker_id(&self) -> Option<usize> {
        if let Some(worker) = self.workers.first() {
            return Some(worker.id)
        }

        None
    }

    fn get_last_worker_id(&self) -> Option<usize> {
        if let Some(worker) = self.workers.last() {
            return Some(worker.id)
        }

        None
    }

    fn get_next_worker_id(&self, current_id: usize) -> Option<usize> {
        if current_id >= self.workers.len() { return None; }

        let mut found = false;
        for worker in &self.workers {
            if found { return Some(worker.id) }
            if worker.id == current_id { found = true; }
        }

        None
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        println!("Job done, sending terminate message to all workers.");

        self.clear();
    }
}

struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Message>>>) -> Worker {
        let thread = thread::spawn(move || {
            Worker::launch(id, receiver);
        });

        Worker {
            id,
            thread: Some(thread),
        }
    }

    fn launch(my_id: usize, receiver: Arc<Mutex<mpsc::Receiver<Message>>>) {
        loop {
            let mut new_job: Option<Job> = None;

            if let Ok(rx) = receiver.lock() {
                if let Ok(message) = rx.recv() {
                    match message {
                        Message::NewJob(job) => new_job = Some(job),
                        Message::Terminate(id) => {
                            if id == 0 { break; }
                            if my_id == id { break; }
                        }
                    }
                }
            }

            if let Some(job) = new_job {
                job.call_box();
            }
        }
    }
}