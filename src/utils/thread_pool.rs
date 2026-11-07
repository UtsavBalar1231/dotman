use once_cell::sync::OnceCell;
use rayon::ThreadPoolBuilder;
use std::sync::Arc;

/// Global thread pool singleton for parallel operations
static THREAD_POOL: OnceCell<Arc<rayon::ThreadPool>> = OnceCell::new();

/// Initialize the global thread pool with the specified number of threads
///
/// If the thread pool is already initialized, this function succeeds silently.
/// This makes the function idempotent and safe to call multiple times.
///
/// # Errors
///
/// Returns an error if the thread pool cannot be built
pub fn init_thread_pool(num_threads: usize) -> anyhow::Result<()> {
    // If already initialized, succeed silently
    if THREAD_POOL.get().is_some() {
        return Ok(());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .thread_name(|i| format!("dotman-worker-{i}"))
        .build()?;

    // Try to set, but don't error if already set (race condition in concurrent init)
    let _ = THREAD_POOL.set(Arc::new(pool));

    Ok(())
}

/// Get the global thread pool, initializing with default settings if needed
///
/// # Panics
///
/// Panics if the thread pool cannot be created
#[allow(clippy::expect_used)] // Documented panic - thread pool creation is critical
pub fn get_thread_pool() -> Arc<rayon::ThreadPool> {
    THREAD_POOL
        .get_or_init(|| {
            let num_threads = num_cpus::get().min(8);
            let pool = ThreadPoolBuilder::new()
                .num_threads(num_threads)
                .thread_name(|i| format!("dotman-worker-{i}"))
                .build()
                .expect("Failed to create thread pool");
            Arc::new(pool)
        })
        .clone()
}

/// Run a function in the configured thread pool
pub fn run_in_pool<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    let pool = get_thread_pool();
    pool.install(f)
}

/// Configure and use the thread pool from config
///
/// # Errors
///
/// Returns an error if the thread pool has already been initialized
pub fn configure_from_config(config: &crate::config::Config) -> anyhow::Result<()> {
    if config.performance.parallel_threads > 0 {
        init_thread_pool(config.performance.parallel_threads)?;
    }
    Ok(())
}

// Re-export for backward compatibility with existing code
pub use rayon::prelude::*;

/// CPU detection module for determining optimal thread count
mod num_cpus {
    use std::sync::LazyLock;

    /// Cached number of available CPU cores
    static NUM_CPUS: LazyLock<usize> = LazyLock::new(|| {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    });

    /// Get the number of available CPU cores
    pub fn get() -> usize {
        *NUM_CPUS
    }
}
