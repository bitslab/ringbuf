use std::ptr::addr_of_mut;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Condvar, Mutex,
};

pub mod mutex_spin;
use mutex_spin::SpinFirst;

#[allow(non_upper_case_globals)]
#[inline(always)]
fn rdtscp() -> u64 {
    static mut dummy: u32 = 0;
    unsafe { core::arch::x86_64::__rdtscp(addr_of_mut!(dummy)) }
}

/// Spins for a given number of cycles checking a condition, then sleeps using the `Condvar` if the condition is not met.
///
/// # Parameters
/// - `guard`: A `MutexGuard` for mutual exclusion.
/// - `condvar`: A `Condvar` used to put the thread to sleep if the condition is not met after `cycles` spins.
/// - `condition`: A closure returning a `bool` indicating whether the condition is met.
/// - `no_sleep_condition`: A closure returning a `bool` indicating whether to never fall asleep and just keep spinning
/// - `cycles`: The number of cycles to spin before sleeping.
///
/// # Type Parameters
/// - `T`: The type of the data protected by `guard`.
/// - `F`: The closure type of `condition`.
pub fn check_and_sleep<T, F: FnMut(&T) -> bool, R: FnMut(&T) -> bool>(
    mutex: &Mutex<T>,
    condvar: &Condvar,
    mut condition: F,
    mut no_sleep_condition: R,
    cycles: u64,
    num_sleepers: &AtomicUsize,
) {
    const SPIN_FIRST_CYCLES: u64 = 2_000_000;

    let start = crate::rdtscp();

    let mut no_sleep;
    // Spin loop
    loop {
        {
            let guard = mutex.spin_lock().unwrap();
            if condition(&*guard) {
                // Didn't need to sleep. Condition met
                return;
            }

            no_sleep = no_sleep_condition(&*guard);
        }

        if !no_sleep && crate::rdtscp() > start + cycles {
            // We've exceeded our spin cycles. Time to sleep
            break;
        }

        for _ in 0..2 {
            std::hint::spin_loop(); // x86-64 "pause" instruction
        }
    }

    // Default to standard OS condvar sleeping
    num_sleepers.fetch_add(1, Ordering::Acquire);

    // As reader: Have to check ourselves (not kernel) that the SavedBuffer is still Reader
    // If it is still reader, we will use a futex call to maybe go to sleep, telling futex that we
    // don't want to sleep if the discriminant of the SavedBuffer enum has changed from the Reader state.

    // No more condvar here.

    let mut guard = mutex.spin_lock().unwrap();
    while !condition(&*guard) {
        guard = condvar.wait(guard).unwrap();
    }
    num_sleepers.fetch_sub(1, Ordering::Acquire);
}

// TODO:(Jacob) Implement some tests
#[cfg(test)]
mod tests {}
