//! Minimal job executor: a FIFO of promise jobs that checks the job deadline
//! between jobs. Timers are implemented in the prelude and driven by the
//! worker, so boa's own timeout jobs are not needed. Async jobs only come
//! from module loading, whose loader answers synchronously, so they finish
//! on their first poll.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Poll, Waker};

use boa_engine::job::{Job, JobExecutor, NativeAsyncJob};
use boa_engine::{Context, JsNativeError, JsResult};

use crate::host;

/// Polls an async job gets before it counts as stuck.
const ASYNC_POLL_LIMIT: usize = 1_000;

#[derive(Default)]
pub(crate) struct Executor {
    queue: RefCell<VecDeque<Job>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
}

impl Executor {
    pub(crate) fn clear(&self) {
        self.queue.borrow_mut().clear();
        self.async_jobs.borrow_mut().clear();
    }

    /// Runs pending async jobs (module loading) without touching promise
    /// jobs, so `pm.require` can load a module graph synchronously.
    pub(crate) fn run_async_jobs(&self, context: &mut Context) -> JsResult<()> {
        loop {
            let Some(job) = self.async_jobs.borrow_mut().pop_front() else { return Ok(()) };
            let cell = RefCell::new(&mut *context);
            let mut fut = pin!(job.call(&cell));
            let mut cx = TaskContext::from_waker(Waker::noop());
            let mut polls = 0;
            let result = loop {
                if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
                    break r;
                }
                polls += 1;
                if polls >= ASYNC_POLL_LIMIT {
                    break Err(JsNativeError::error().with_message("module loading did not complete").into());
                }
            };
            if let Err(e) = result {
                self.async_jobs.borrow_mut().clear();
                return Err(e);
            }
        }
    }
}

impl JobExecutor for Executor {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        match job {
            Job::AsyncJob(j) => self.async_jobs.borrow_mut().push_back(j),
            other => self.queue.borrow_mut().push_back(other),
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        loop {
            host::check_deadline(context)?;
            if !self.async_jobs.borrow().is_empty() {
                self.run_async_jobs(context)?;
            }
            let Some(job) = self.queue.borrow_mut().pop_front() else { return Ok(()) };
            let result = match job {
                Job::PromiseJob(j) => j.call(context).map(|_| ()),
                Job::GenericJob(j) => j.call(context).map(|_| ()),
                Job::TimeoutJob(j) => j.call(context).map(|_| ()),
                // Finalization jobs are never created by the sandbox.
                _ => Ok(()),
            };
            if let Err(e) = result {
                // Promise jobs handle their own errors; anything reaching us
                // is an uncatchable engine error (limits, timeout).
                self.clear();
                return Err(e);
            }
        }
    }
}
