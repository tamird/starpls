use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use rayon::ThreadPool;
use rayon::ThreadPoolBuilder;

pub(crate) struct TaskPool<T> {
    sender: Sender<T>,
    inner: ThreadPool,
}

impl<T> TaskPool<T> {
    pub(crate) fn with_num_threads(
        sender: Sender<T>,
        num_threads: usize,
    ) -> anyhow::Result<TaskPool<T>> {
        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .stack_size(ruff_db::STACK_SIZE)
            .build()?;
        Ok(TaskPool {
            sender,
            inner: thread_pool,
        })
    }

    pub(crate) fn spawn<F>(&self, f: F)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.inner.spawn({
            let sender = self.sender.clone();
            move || {
                // A running job can outlive the server's event receiver.
                let _ = sender.send(f());
            }
        })
    }

    #[allow(unused)]
    pub(crate) fn spawn_with_sender<F>(&self, f: F)
    where
        T: Send + 'static,
        F: FnOnce(Sender<T>) + Send + 'static,
    {
        self.inner.spawn({
            let sender = self.sender.clone();
            move || f(sender)
        })
    }
}

pub(crate) struct TaskPoolHandle<T> {
    pub(crate) receiver: Receiver<T>,
    pool: TaskPool<T>,
}

impl<T> TaskPoolHandle<T> {
    pub(crate) fn new(receiver: Receiver<T>, pool: TaskPool<T>) -> Self {
        Self { receiver, pool }
    }

    pub(crate) fn spawn<F>(&self, f: F)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.pool.spawn(f)
    }

    #[allow(unused)]
    pub(crate) fn spawn_with_sender<F>(&self, f: F)
    where
        T: Send + 'static,
        F: FnOnce(Sender<T>) + Send + 'static,
    {
        self.pool.spawn_with_sender(f)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::TaskPool;

    #[test]
    fn running_jobs_outlive_the_completion_receiver() {
        for panic_in_job in [false, true] {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let (panic_sender, panics) = crossbeam_channel::unbounded();
            let (exit_sender, exited) = crossbeam_channel::bounded(1);
            let pool = TaskPool {
                sender,
                inner: rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .panic_handler(move |_| {
                        panic_sender.send(()).unwrap();
                    })
                    .exit_handler(move |_| {
                        exit_sender.send(()).unwrap();
                    })
                    .build()
                    .unwrap(),
            };
            let (entered, entering) = crossbeam_channel::bounded(1);
            let (resume, resumed) = crossbeam_channel::bounded(1);
            pool.spawn(move || {
                entered.send(()).unwrap();
                resumed.recv().unwrap();
                assert!(!panic_in_job, "the job's own panic must still be reported");
            });
            entering.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(receiver);
            drop(pool);
            resume.send(()).unwrap();
            exited.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(panics.try_iter().count(), usize::from(panic_in_job));
        }
    }
}
