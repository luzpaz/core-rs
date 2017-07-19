//! Thredder is a thread tracking system that not only creates threads for
//! specific purposes, but sets up communication channels between the threads
//! and tracks the state of them.

use ::std::marker::Send;

use ::futures::Future;
use ::futures_cpupool::CpuPool;

use ::error::{TResult, TFutureResult};

/// Stores state information for a thread we've spawned.
///
/// NOTE: Thredder used to have a lot of wrapping around CpuPool and provided a
/// lot of utilities for passing data between pools and our main thread. Those
/// days are gone now since many improvements to CpuPool, so it now exists as a
/// very thin layer.
pub struct Thredder {
    /// Our Thredder's name
    pub name: String,
    /// Stores the thread pooler for this Thredder
    pool: CpuPool,
}

impl Thredder {
    /// Create a new thredder
    pub fn new(name: &str, workers: u32) -> Thredder {
        Thredder {
            name: String::from(name),
            pool: CpuPool::new(workers as usize),
        }
    }

    /// Run an operation on this pool, returning the Future to be waited on at
    /// a later time.
    pub fn run_async<F, T>(&self, run: F) -> TFutureResult<T>
        where T: Sync + Send + 'static,
              F: FnOnce() -> TResult<T> + Send + 'static
    {
        self.pool.spawn_fn(run).boxed()
    }

    /// Run an operation on this pool
    pub fn run<F, T>(&self, run: F) -> TResult<T>
        where T: Sync + Send + 'static,
              F: FnOnce() -> TResult<T> + Send + 'static
    {
        self.pool.spawn_fn(run).wait()
    }
}

