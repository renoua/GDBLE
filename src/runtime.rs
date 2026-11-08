use std::sync::Mutex;
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};

/// RuntimeManager is the SINGLE owner of the Tokio runtime.
///
/// Everything else (scanner, devices) only gets a [`Handle`]: a handle never owns the worker
/// threads, so no Godot- or Tokio-side code can end up dropping the runtime (a multi-thread
/// `Runtime` drop waits for its workers with no upper bound, and panics when dropped from
/// inside an async context). Shutdown is explicit and bounded: see [`RuntimeManager::shutdown`].
pub struct RuntimeManager {
    runtime: Mutex<Option<Runtime>>,
    handle: Handle,
}

impl RuntimeManager {
    /// Creates a new RuntimeManager with a multi-threaded Tokio runtime
    ///
    /// # Panics
    /// Panics if the Tokio runtime cannot be created
    pub fn new() -> Self {
        let runtime = Self::build_runtime();
        let handle = runtime.handle().clone();
        Self {
            runtime: Mutex::new(Some(runtime)),
            handle,
        }
    }

    #[cfg(target_os = "android")]
    fn build_runtime() -> Runtime {
        // droidplug calls into the JVM from async tasks: every worker thread must be attached.
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .on_thread_start(crate::android::attach_current_thread)
            .build()
            .expect("Failed to create Tokio runtime")
    }

    #[cfg(not(target_os = "android"))]
    fn build_runtime() -> Runtime {
        Runtime::new().expect("Failed to create Tokio runtime")
    }

    /// Returns a handle to the runtime (cheap to clone, does not keep the runtime alive).
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Spawns an async task on the runtime. After [`shutdown`](Self::shutdown) the task is
    /// dropped without running.
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let _ = self.handle.spawn(future);
    }

    /// Blocks the current thread until the future completes
    ///
    /// This method should be used sparingly as it blocks the calling thread.
    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: std::future::Future,
    {
        self.handle.block_on(future)
    }

    /// Stops the runtime, waiting at most `timeout` for running tasks. Idempotent.
    /// Must not be called from inside the runtime.
    pub fn shutdown(&self, timeout: Duration) {
        let runtime = match self.runtime.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(runtime) = runtime {
            runtime.shutdown_timeout(timeout);
        }
    }
}

impl Drop for RuntimeManager {
    fn drop(&mut self) {
        // Never block while the Godot scene tree is torn down.
        let runtime = match self.runtime.get_mut() {
            Ok(slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(runtime) = runtime {
            runtime.shutdown_background();
        }
    }
}

impl Default for RuntimeManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_runtime_manager_creation() {
        let manager = RuntimeManager::new();
        assert!(manager.runtime.lock().unwrap().is_some());
    }

    #[test]
    fn test_shutdown_is_bounded_with_a_task_that_never_ends() {
        let manager = RuntimeManager::new();
        // A task parked on a blocking sleep cannot be cancelled by the runtime.
        manager.spawn(async {
            std::thread::sleep(Duration::from_secs(30));
        });
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        manager.shutdown(Duration::from_millis(200));
        assert!(start.elapsed() < Duration::from_secs(2), "shutdown must be bounded");
        // Idempotent, and spawning afterwards must not panic.
        manager.shutdown(Duration::from_millis(10));
        manager.spawn(async {});
    }

    #[test]
    fn test_drop_does_not_block() {
        let manager = RuntimeManager::new();
        manager.spawn(async {
            std::thread::sleep(Duration::from_secs(30));
        });
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        drop(manager);
        assert!(start.elapsed() < Duration::from_secs(2), "drop must not wait for workers");
    }

    #[test]
    fn test_spawn_execution() {
        let manager = RuntimeManager::new();
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        manager.spawn(async move {
            flag_clone.store(true, Ordering::SeqCst);
        });

        // Give the task time to execute
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_block_on_execution() {
        let manager = RuntimeManager::new();

        let result = manager.block_on(async { 42 });

        assert_eq!(result, 42);
    }

    #[test]
    fn test_block_on_with_computation() {
        let manager = RuntimeManager::new();

        let result = manager.block_on(async {
            let mut sum = 0;
            for i in 1..=10 {
                sum += i;
            }
            sum
        });

        assert_eq!(result, 55);
    }
}
