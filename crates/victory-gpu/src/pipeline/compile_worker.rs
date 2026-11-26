//! One reusable Windows thread for a caller's synchronous host compiles.
use std::os::windows::io::AsRawHandle;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, WaitForSingleObject,
};

pub(super) struct Worker<J, R> {
    jobs: mpsc::Sender<J>,
    results: mpsc::Receiver<R>,
    thread: JoinHandle<()>,
}

impl<J: Send + 'static, R: Send + 'static> Worker<J, R> {
    pub(super) fn spawn(compile: impl Fn(J) -> R + Send + 'static) -> std::io::Result<Self> {
        let (jobs, requests) = mpsc::channel();
        let (replies, results) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("victory-pipeline-compile".into())
            .stack_size(8 << 20)
            .spawn(move || {
                // SAFETY: changes the priority of this thread only.
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                }
                for job in requests {
                    if replies.send(compile(job)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            jobs,
            results,
            thread,
        })
    }

    pub(super) fn run(&mut self, job: J) -> Option<R> {
        self.jobs.send(job).ok()?;
        loop {
            match self.results.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return Some(result),
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // A native exit can leave the Rust sender alive. A healthy compile
                    // wakes the receiver as soon as the result is ready.
                    // SAFETY: the JoinHandle owns the native handle throughout the wait.
                    if unsafe { WaitForSingleObject(HANDLE(self.thread.as_raw_handle()), 0) }
                        != WAIT_TIMEOUT
                    {
                        return self.results.try_recv().ok();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successive_compiles_run_on_the_same_thread_and_deliver_distinct_replies() {
        let mut worker = Worker::spawn(|value: i32| (thread::current().id(), value + 1)).unwrap();
        let (id, answer) = worker.run(10).unwrap();
        assert_ne!(id, thread::current().id());
        assert_eq!(answer, 11);
        for value in 0..256 {
            assert_eq!(worker.run(value), Some((id, value + 1)));
        }
    }

    #[test]
    fn worker_exit_releases_the_waiter_and_a_new_worker_can_run() {
        let mut worker = Worker::spawn(|stop: bool| {
            assert!(!stop, "test worker exit");
            17
        })
        .unwrap();
        assert_eq!(worker.run(false), Some(17));
        assert_eq!(worker.run(true), None);
        drop(worker);
        let mut replacement = Worker::spawn(|value| value).unwrap();
        assert_eq!(replacement.run(42), Some(42));
    }
    #[test]
    fn a_signalled_thread_handle_finishes_a_request_even_when_the_sender_survives() {
        let (jobs, _requests) = mpsc::channel();
        let (_live_sender, results) = mpsc::channel::<usize>();
        let thread = thread::spawn(|| {});
        let mut worker = Worker {
            jobs,
            results,
            thread,
        };
        assert_eq!(worker.run(()), None);
    }

    #[test]
    #[ignore = "measure thread dispatch overhead on the target Windows runtime"]
    fn compare_compile_thread_dispatch() {
        use std::time::Instant;
        use windows::Win32::System::Threading::INFINITE;
        const JOBS: u32 = 128;
        for pair in 0..7 {
            for reuse in [pair % 2 != 0, pair % 2 == 0] {
                let started = Instant::now();
                let mut worker = reuse.then(|| Worker::spawn(|i: u32| i + 1).unwrap());
                for i in 0..JOBS {
                    let answer = if let Some(worker) = &mut worker {
                        worker.run(i).unwrap()
                    } else {
                        let (tx, rx) = mpsc::channel();
                        let handle = thread::Builder::new()
                            .name("victory-pipeline-compile".into())
                            .stack_size(8 << 20)
                            .spawn(move || {
                                // SAFETY: the same priority as an isolated host compile.
                                unsafe {
                                    let _ = SetThreadPriority(
                                        GetCurrentThread(),
                                        THREAD_PRIORITY_BELOW_NORMAL,
                                    );
                                }
                                tx.send(i + 1).unwrap();
                            })
                            .unwrap();
                        // SAFETY: the thread owns a live handle throughout the wait.
                        unsafe {
                            WaitForSingleObject(HANDLE(handle.as_raw_handle()), INFINITE);
                        }
                        rx.recv().unwrap()
                    };
                    assert_eq!(answer, i + 1);
                }
                println!(
                    "compile dispatch pair={pair} reuse={reuse} jobs={JOBS} us={}",
                    started.elapsed().as_micros()
                );
            }
        }
    }
}
