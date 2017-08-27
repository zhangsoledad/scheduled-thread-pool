//! A thread pool to execute scheduled actions in parallel.
//!
//! While a normal thread pool is only able to execute actions as soon as
//! possible, a scheduled thread pool can execute actions after a specific
//! delay, or excecute actions periodically.
#![warn(missing_docs)]
#![doc(html_root_url="https://docs.rs/scheduled-thread-pool/0.1.0")]

extern crate parking_lot;

use parking_lot::{Mutex, Condvar};
use std::collections::BinaryHeap;
use std::cmp::{PartialOrd, Ord, PartialEq, Eq, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::panic::{self, AssertUnwindSafe};

use thunk::Thunk;

mod thunk;

enum JobType {
    Once(Thunk<'static>),
    FixedRate {
        f: Box<FnMut() + Send + 'static>,
        rate: Duration,
    },
    FixedDelay {
        f: Box<FnMut() + Send + 'static>,
        delay: Duration,
    },
}

struct Job {
    type_: JobType,
    time: Instant,
}

impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Job) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Job {
    fn cmp(&self, other: &Job) -> Ordering {
        // reverse because BinaryHeap's a max heap
        self.time.cmp(&other.time).reverse()
    }
}

impl PartialEq for Job {
    fn eq(&self, other: &Job) -> bool {
        self.time == other.time
    }
}

impl Eq for Job {}

struct InnerPool {
    queue: BinaryHeap<Job>,
    shutdown: bool,
}

struct SharedPool {
    inner: Mutex<InnerPool>,
    cvar: Condvar,
}

impl SharedPool {
    fn run(&self, job: Job) {
        let mut inner = self.inner.lock();

        // Calls from the pool itself will never hit this, but calls from workers might
        if inner.shutdown {
            return;
        }

        match inner.queue.peek() {
            None => self.cvar.notify_all(),
            Some(e) if e.time > job.time => self.cvar.notify_all(),
            _ => {}
        };
        inner.queue.push(job);
    }
}

/// A pool of threads which can run tasks at specific time intervals.
///
/// When the pool drops, all pending scheduled executions will be run, but
/// periodic actions will not be rescheduled after that.
pub struct ScheduledThreadPool {
    shared: Arc<SharedPool>,
}

impl Drop for ScheduledThreadPool {
    fn drop(&mut self) {
        self.shared.inner.lock().shutdown = true;
        self.shared.cvar.notify_all();
    }
}

impl ScheduledThreadPool {
    /// Creates a new thread pool with the specified number of threads.
    ///
    /// # Panics
    ///
    /// Panics if `num_threads` is 0.
    pub fn new(num_threads: usize) -> ScheduledThreadPool {
        ScheduledThreadPool::new_inner(None, num_threads)
    }

    /// Creates a new thread pool with the specified number of threads which
    /// will be named.
    ///
    /// The substring `{}` in the name will be replaced with an integer
    /// identifier of the thread.
    ///
    /// # Panics
    ///
    /// Panics if `num_threads` is 0.
    pub fn with_name(thread_name: &str, num_threads: usize) -> ScheduledThreadPool {
        ScheduledThreadPool::new_inner(Some(thread_name), num_threads)
    }

    fn new_inner(thread_name: Option<&str>, num_threads: usize) -> ScheduledThreadPool {
        assert!(num_threads > 0, "num_threads must be positive");

        let inner = InnerPool {
            queue: BinaryHeap::new(),
            shutdown: false,
        };

        let shared = SharedPool {
            inner: Mutex::new(inner),
            cvar: Condvar::new(),
        };

        let pool = ScheduledThreadPool { shared: Arc::new(shared) };

        for i in 0..num_threads {
            Worker::start(
                thread_name.map(|n| n.replace("{}", &i.to_string())),
                pool.shared.clone(),
            );
        }

        pool
    }

    /// Executes a closure as soon as possible in the pool.
    pub fn execute<F>(&self, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.execute_after(Duration::from_secs(0), job)
    }

    /// Executes a closure after a time delay in the pool.
    pub fn execute_after<F>(&self, delay: Duration, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let job = Job {
            type_: JobType::Once(Thunk::new(job)),
            time: Instant::now() + delay,
        };
        self.shared.run(job)
    }

    /// Executes a closure after an initial delay at a fixed rate in the pool.
    ///
    /// The rate includes the time spent running the closure. For example, if
    /// the rate is 5 seconds and the closure takes 2 seconds to run, the
    /// closure will be run again 3 seconds after it completes.
    ///
    /// # Panics
    ///
    /// If the closure panics, it will not be run again.
    pub fn execute_at_fixed_rate<F>(&self, initial_delay: Duration, rate: Duration, f: F)
    where
        F: FnMut() + Send + 'static,
    {
        let job = Job {
            type_: JobType::FixedRate {
                f: Box::new(f),
                rate: rate,
            },
            time: Instant::now() + initial_delay,
        };
        self.shared.run(job)
    }

    /// Executes a closure after an initial delay at a fixed rate in the pool.
    ///
    /// In contrast to `execute_at_fixed_rate`, the execution time of the
    /// closure is not subtracted from the delay before it runs again. For
    /// example, if the delay is 5 seconds and the closure takes 2 seconds to
    /// run, the closure will run again 5 seconds after it completes.
    ///
    /// # Panics
    ///
    /// If the closure panics, it will not be run again.
    pub fn execute_with_fixed_delay<F>(&self, initial_delay: Duration, delay: Duration, f: F)
    where
        F: FnMut() + Send + 'static,
    {
        let job = Job {
            type_: JobType::FixedDelay {
                f: Box::new(f),
                delay: delay,
            },
            time: Instant::now() + initial_delay,
        };
        self.shared.run(job)
    }
}

struct Worker {
    shared: Arc<SharedPool>,
}

impl Worker {
    fn start(name: Option<String>, shared: Arc<SharedPool>) {
        let mut worker = Worker { shared: shared };

        let mut thread = thread::Builder::new();
        if let Some(name) = name {
            thread = thread.name(name);
        }
        thread.spawn(move || worker.run()).unwrap();
    }

    fn run(&mut self) {
        while let Some(job) = self.get_job() {
            // we don't reschedule jobs after they panic, so this is safe
            let _ = panic::catch_unwind(AssertUnwindSafe(|| self.run_job(job)));
        }
    }

    fn get_job(&self) -> Option<Job> {
        enum Need {
            Wait,
            WaitTimeout(Duration),
        }

        let mut inner = self.shared.inner.lock();
        loop {
            let now = Instant::now();

            let need = match inner.queue.peek() {
                None if inner.shutdown => return None,
                None => Need::Wait,
                Some(e) if e.time <= now => break,
                Some(e) => Need::WaitTimeout(e.time - now),
            };

            match need {
                Need::Wait => {
                    self.shared.cvar.wait(&mut inner);
                }
                Need::WaitTimeout(t) => {
                    self.shared.cvar.wait_for(&mut inner, t);
                }
            }
        }

        Some(inner.queue.pop().unwrap())
    }

    fn run_job(&self, job: Job) {
        match job.type_ {
            JobType::Once(f) => f.invoke(()),
            JobType::FixedRate { mut f, rate } => {
                f();
                let new_job = Job {
                    type_: JobType::FixedRate { f: f, rate: rate },
                    time: job.time + rate,
                };
                self.shared.run(new_job)
            }
            JobType::FixedDelay { mut f, delay } => {
                f();
                let new_job = Job {
                    type_: JobType::FixedDelay { f: f, delay: delay },
                    time: Instant::now() + delay,
                };
                self.shared.run(new_job)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use super::ScheduledThreadPool;

    const TEST_TASKS: usize = 4;

    #[test]
    fn test_works() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);

        let (tx, rx) = channel();
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            pool.execute(move || { tx.send(1usize).unwrap(); });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).fold(0, |a, b| a + b), TEST_TASKS);
    }

    #[test]
    #[should_panic(expected = "num_threads must be positive")]
    fn test_zero_tasks_panic() {
        ScheduledThreadPool::new(0);
    }

    #[test]
    fn test_recovery_from_subtask_panic() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);

        // Panic all the existing threads.
        let waiter = Arc::new(Barrier::new(TEST_TASKS as usize));
        for _ in 0..TEST_TASKS {
            let waiter = waiter.clone();
            pool.execute(move || -> () {
                waiter.wait();
                panic!();
            });
        }

        // Ensure the pool still works.
        let (tx, rx) = channel();
        let waiter = Arc::new(Barrier::new(TEST_TASKS as usize));
        for _ in 0..TEST_TASKS {
            let tx = tx.clone();
            let waiter = waiter.clone();
            pool.execute(move || {
                waiter.wait();
                tx.send(1usize).unwrap();
            });
        }

        assert_eq!(rx.iter().take(TEST_TASKS).fold(0, |a, b| a + b), TEST_TASKS);
    }

    #[test]
    fn test_execute_after() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);
        let (tx, rx) = channel();

        let tx1 = tx.clone();
        pool.execute_after(Duration::from_secs(1), move || tx1.send(1usize).unwrap());
        pool.execute_after(Duration::from_millis(500), move || tx.send(2usize).unwrap());

        assert_eq!(2, rx.recv().unwrap());
        assert_eq!(1, rx.recv().unwrap());
    }

    #[test]
    fn test_jobs_complete_after_drop() {
        let pool = ScheduledThreadPool::new(TEST_TASKS);
        let (tx, rx) = channel();

        let tx1 = tx.clone();
        pool.execute_after(Duration::from_secs(1), move || tx1.send(1usize).unwrap());
        pool.execute_after(Duration::from_millis(500), move || tx.send(2usize).unwrap());

        drop(pool);

        assert_eq!(2, rx.recv().unwrap());
        assert_eq!(1, rx.recv().unwrap());
    }

    #[test]
    fn test_fixed_delay_jobs_stop_after_drop() {
        let pool = Arc::new(ScheduledThreadPool::new(TEST_TASKS));
        let (tx, rx) = channel();
        let (tx2, rx2) = channel();

        let mut pool2 = Some(pool.clone());
        let mut i = 0i32;
        pool.execute_at_fixed_rate(
            Duration::from_millis(500),
            Duration::from_millis(500),
            move || {
                i += 1;
                tx.send(i).unwrap();
                rx2.recv().unwrap();
                if i == 2 {
                    drop(pool2.take().unwrap());
                }
            },
        );
        drop(pool);

        assert_eq!(Ok(1), rx.recv());
        tx2.send(()).unwrap();
        assert_eq!(Ok(2), rx.recv());
        tx2.send(()).unwrap();
        assert!(rx.recv().is_err());
    }
}
