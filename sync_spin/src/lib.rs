use std::ptr::addr_of_mut;
use std::sync::{Condvar, Mutex, MutexGuard};

pub mod mutex_spin;
use mutex_spin::SpinFirst;

#[allow(non_upper_case_globals)]
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
/// - `cycles`: The number of cycles to spin before sleeping.
///
/// # Type Parameters
/// - `T`: The type of the data protected by `guard`.
/// - `F`: The closure type of `condition`.
pub fn check_and_sleep<T, F: Fn(&T) -> bool>(
    mutex: Mutex<T>,
    condvar: &Condvar,
    condition: F,
    cycles: u64,
) {
    const SPIN_FIRST_CYCLES: u64 = 10_000; //TODO:(Jacob) Evaluate this choice of number

    let start = crate::rdtscp();

    // Spin loop
    loop {
        {
            let guard = mutex.spin_first_lock(SPIN_FIRST_CYCLES).unwrap();
            if condition(&*guard) {
                // Didn't need to sleep. Condition met
                return;
            }
        }

        if crate::rdtscp() > start + cycles {
            // We've exceeded our spin cycles. Time to sleep
            break;
        }

        std::hint::spin_loop(); // x86-64 "pause" instruction
    }

    // Default to standard OS condvar sleeping
    let mut guard = mutex.spin_first_lock(SPIN_FIRST_CYCLES).unwrap();
    while !condition(&*guard) {
        guard = condvar.wait(guard).unwrap();
    }
}

// TODO:(Jacob) Implement some tests
#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn test_one() {}
}
