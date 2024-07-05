use std::sync::{LockResult, Mutex, MutexGuard};

/// A trait to provide a customizable spin-first lock mechanism for [`Mutex`].
pub trait SpinFirst<Mutex> {
    /// Attempts to lock the mutex with a spin loop for a specified number of CPU cycles, before
    /// falling back to a standard Mutex::lock call (which will perform a `futex` syscall).
    ///
    /// # Parameters
    /// - `spin_cycles`: The number of CPU cycles to spin before falling back to
    /// [`Mutex::lock`](std::sync::Mutex::lock).
    ///
    /// # Returns
    /// A [`LockResult`] which is:
    /// - `Ok(MutexGuard)` if the lock was successfully acquired.
    /// - `Err(PoisonError<Guard>)` if the lock is poisoned.
    fn spin_first_lock(&self, spin_cycles: u64) -> LockResult<MutexGuard<'_, Mutex>>;
}

impl<T> SpinFirst<T> for Mutex<T> {
    fn spin_first_lock(&self, spin_cycles: u64) -> LockResult<MutexGuard<'_, T>> {
        let start_cycles = crate::rdtscp();
        while crate::rdtscp() < start_cycles + spin_cycles {
            for _ in 0..5 {
                std::hint::spin_loop();
            }
            match self.try_lock() {
                Ok(lock) => return Ok(lock),
                Err(std::sync::TryLockError::WouldBlock) => {}
                Err(std::sync::TryLockError::Poisoned(e)) => return Err(e),
            }
        }
        self.lock()
    }
}
