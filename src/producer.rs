use alloc::sync::Arc;
use core::{
    mem::{self, MaybeUninit},
    ptr::copy_nonoverlapping,
    sync::atomic::Ordering,
};
use std::cmp;
#[cfg(feature = "std")]
use std::io::{self, Read, Write};
use std::mem::size_of;
use sync_spin::{check_and_sleep, mutex_spin::SpinFirst};

use crate::{consumer::Consumer, ring_buffer::*};

/// Producer part of ring buffer.
pub struct Producer<T> {
    pub(crate) rb: Arc<RingBuffer<T>>,
    pub(crate) nonblocking: bool,
}

impl<T: Sized> Producer<T> {
    /// Returns capacity of the ring buffer.
    ///
    /// The capacity of the buffer is constant.
    pub fn capacity(&self) -> usize {
        self.rb.capacity()
    }

    /// Checks if the ring buffer is empty.
    ///
    /// The result is relevant until you push items to the producer.
    pub fn is_empty(&self) -> bool {
        self.rb.is_empty()
    }

    /// Checks if the ring buffer is full.
    ///
    /// *The result may become irrelevant at any time because of concurring activity of the consumer.*
    pub fn is_full(&self) -> bool {
        self.rb.is_full()
    }

    /// The length of the data stored in the buffer.
    ///
    /// Actual length may be equal to or less than the returned value.
    pub fn len(&self) -> usize {
        self.rb.len()
    }

    /// Checks if the consumer end is still present.
    pub fn is_consumer_alive(&self) -> bool {
        Arc::strong_count(&self.rb) >= 2
    }

    pub fn set_nonblocking(&mut self) {
        self.nonblocking = true;
    }

    /// The remaining space in the buffer.
    ///
    /// Actual remaining space may be equal to or greater than the returning value.
    pub fn remaining(&self) -> usize {
        self.rb.remaining()
    }

    /// Allows to write into ring buffer memory directry.
    ///
    /// *This function is unsafe because it gives access to possibly uninitialized memory*
    ///
    /// The method takes a function `f` as argument.
    /// `f` takes two slices of ring buffer content (the second one or both of them may be empty).
    /// First slice contains older elements.
    ///
    /// `f` should return number of elements been written.
    /// *There is no checks for returned number - it remains on the developer's conscience.*
    ///
    /// The method **always** calls `f` even if ring buffer is full.
    ///
    /// The method returns number returned from `f`.
    ///
    /// # Safety
    ///
    /// The method gives access to ring buffer underlying memory which may be uninitialized.
    ///
    pub unsafe fn push_access<F>(&self, f: F) -> usize
    where
        F: FnOnce(&mut [MaybeUninit<T>], &mut [MaybeUninit<T>]) -> usize,
    {
        let head = self.rb.head.load(Ordering::Acquire);
        let tail = self.rb.tail.load(Ordering::Acquire);
        let len = self.rb.data.get_ref().len();

        let ranges = if tail >= head {
            if head > 0 {
                (tail..len, 0..(head - 1))
            } else if tail < len - 1 {
                (tail..(len - 1), 0..0)
            } else {
                (0..0, 0..0)
            }
        } else if tail < head - 1 {
            (tail..(head - 1), 0..0)
        } else {
            (0..0, 0..0)
        };

        let slices = (
            &mut self.rb.data.get_mut()[ranges.0],
            &mut self.rb.data.get_mut()[ranges.1],
        );

        let n = f(slices.0, slices.1);

        if n > 0 {
            let new_tail = (tail + n) % len;
            self.rb.tail.store(new_tail, Ordering::Release);
        }
        n
    }

    /// Copies data from the slice to the ring buffer in byte-to-byte manner.
    ///
    /// The `elems` slice should contain **initialized** data before the method call.
    /// After the call the copied part of data in `elems` should be interpreted as **un-initialized**.
    ///
    /// Returns the number of items been copied.
    ///
    /// # Safety
    ///
    /// The method copies raw data into the ring buffer.
    ///
    /// *You should properly fill the slice and manage remaining elements after copy.*
    ///
    pub unsafe fn push_copy(&self, elems: &[MaybeUninit<T>]) -> usize {
        self.push_access(|left, right| -> usize {
            if elems.len() < left.len() {
                copy_nonoverlapping(elems.as_ptr(), left.as_mut_ptr(), elems.len());
                elems.len()
            } else {
                copy_nonoverlapping(elems.as_ptr(), left.as_mut_ptr(), left.len());
                if elems.len() < left.len() + right.len() {
                    copy_nonoverlapping(
                        elems.as_ptr().add(left.len()),
                        right.as_mut_ptr(),
                        elems.len() - left.len(),
                    );
                    elems.len()
                } else {
                    copy_nonoverlapping(
                        elems.as_ptr().add(left.len()),
                        right.as_mut_ptr(),
                        right.len(),
                    );
                    left.len() + right.len()
                }
            }
        })
    }

    /// Appends an element to the ring buffer.
    /// On failure returns an error containing the element that hasn't beed appended.
    pub fn push(&mut self, elem: T) -> Result<(), T> {
        let mut elem_mu = MaybeUninit::new(elem);
        let n = unsafe {
            self.push_access(|slice, _| {
                if !slice.is_empty() {
                    mem::swap(slice.get_unchecked_mut(0), &mut elem_mu);
                    1
                } else {
                    0
                }
            })
        };
        match n {
            0 => Err(unsafe { elem_mu.assume_init() }),
            1 => Ok(()),
            _ => unreachable!(),
        }
    }

    /// Repeatedly calls the closure `f` and pushes elements returned from it to the ring buffer.
    ///
    /// The closure is called until it returns `None` or the ring buffer is full.
    ///
    /// The method returns number of elements been put into the buffer.
    pub fn push_each<F: FnMut() -> Option<T>>(&mut self, mut f: F) -> usize {
        unsafe {
            self.push_access(|left, right| {
                for (i, dst) in left.iter_mut().enumerate() {
                    match f() {
                        Some(e) => dst.as_mut_ptr().write(e),
                        None => return i,
                    };
                }
                for (i, dst) in right.iter_mut().enumerate() {
                    match f() {
                        Some(e) => dst.as_mut_ptr().write(e),
                        None => return i + left.len(),
                    };
                }
                left.len() + right.len()
            })
        }
    }

    /// Appends elements from an iterator to the ring buffer.
    /// Elements that haven't been added to the ring buffer remain in the iterator.
    ///
    /// Returns count of elements been appended to the ring buffer.
    pub fn push_iter<I: Iterator<Item = T>>(&mut self, elems: &mut I) -> usize {
        self.push_each(|| elems.next())
    }

    /// Removes at most `count` elements from the consumer and appends them to the producer.
    /// If `count` is `None` then as much as possible elements will be moved.
    /// The producer and consumer parts may be of different buffers as well as of the same one.
    ///
    /// On success returns number of elements been moved.
    pub fn move_from(&mut self, other: &mut Consumer<T>, count: Option<usize>) -> usize {
        move_items(other, self, count)
    }
}

impl<T: Sized + Copy> Producer<T> {
    /// Appends elements from slice to the ring buffer.
    /// Elements should be [`Copy`](https://doc.rust-lang.org/std/marker/trait.Copy.html).
    ///
    /// Returns count of elements been appended to the ring buffer.
    pub fn push_slice(&mut self, elems: &[T]) -> usize {
        let mut saved_buf_lock = self.rb.saved_buf.spin_lock().unwrap();

        let mut bytes_written = 0usize;
        if cfg!(feature = "reader-sleep-copy") {
            // We need to check if the reader had a saved buffer
            if let SavedBuffer::<T>::Reader(reader_buf) = *saved_buf_lock {
                // Reader had a saved buffer! Copy to it!
                let reader_slice: &mut [T] = (&reader_buf).into();
                let copy_len = cmp::min(elems.len(), reader_slice.len());
                unsafe {
                    // Perform the copy
                    /* SAFETY: These buffers are in two separate "processes," so the slices should 
                     * NOT have any overlap */
                    std::intrinsics::copy_nonoverlapping(elems.as_ptr(), reader_buf.0, copy_len);
                }

                // Overwrite the enum for the saved buffer we just wrote to with the number of bytes we wrote
                *saved_buf_lock = SavedBuffer::Copied(copy_len); // This way the reader will know we copied the buffer over

                // Update the number of bytes written
                bytes_written += copy_len;

                // Set the number of bytes we copied before waking the reader
                // This will "wake" the reader already if they are just spinning
                // self.rb.size_copied.store(copy_len, Ordering::Relaxed);

                if cfg!(feature = "debug-print") {
                    std::println!(
                        "[RINGBUF DEBUG] Writer copied {copy_len} bytes directly from its buffer to a saved reader's buffer"
                    );
                }

                // If we've written all the bytes we want written, return now
                //if bytes_written == elems.len() {
                return bytes_written;
                //}
            }
        }

        if cfg!(feature = "writer-sleep-copy") {
            use std::ops::DerefMut;
            // We still have more to write.
            if saved_buf_lock.is_none() {
                // Set our buffer if the reader hasn't saved one yet
                *saved_buf_lock = SavedBuffer::Writer(elems.into()); // Grab lock without futex (hopefully faster)

                let num_copied = loop {
                    if let SavedBuffer::CopyingHelpRequest(reader_buf, req_writer_buf, readers_copy_len) = *saved_buf_lock {
                        // Reader requested a copy (it will do a different one in parallel)
                        unsafe {
                            std::intrinsics::copy_nonoverlapping(req_writer_buf.0, reader_buf.0, reader_buf.1);
                        }

                        // Reader will also wait on this barrier after its copy
                        self.rb.barrier.wait();

                        // Assign to `num_copied` the amount we copied plus the amount the reader said they'd copy
                        break reader_buf.1 + readers_copy_len;
                    }

                    if let SavedBuffer::Copied(n) = *saved_buf_lock {
                        *saved_buf_lock = SavedBuffer::None;
                        break n;
                    }

                    // Push 1024 bytes (or whatever number of bytes less than 1024 we have left in our buffer) to the ring buffer
                    let num_bytes_pushing = cmp::min(1024, elems.len() - bytes_written);

                    if let SavedBuffer::Writer(writer_buf) = saved_buf_lock.deref_mut() {
                        // Before dropping the lock, move our saved writer's buffer (+ to ptr, - to size)
                        *writer_buf += num_bytes_pushing;
                    } else {
                        // SAFETY: We just set this enum to SavedBuffer::Writer and the reader hasn't set it to SavedBuffer::Copied yet
                        //unsafe { std::hint::unreachable_unchecked() };
                        unreachable!(); // Just making "sure" this is actually unreachable
                    }

                    // Push a slice while we wait (instead of a "pause" instruction)
                    let index_to_push_until = bytes_written + num_bytes_pushing;
                    unsafe {
                        self.push_copy(
                            &*(&elems[bytes_written..index_to_push_until] as *const [T]
                                as *const [MaybeUninit<T>]),
                        );
                    }

                    bytes_written = index_to_push_until;

                    // If we've pushed everything we could, we can just return right now (after making sure we haven't left our writer buffer there!)
                    if bytes_written == elems.len() {
                        // Make sure to reset the saved writer buffer to None before returning!
                        *saved_buf_lock = SavedBuffer::None;
                        return bytes_written;
                    }

                    drop(saved_buf_lock); // It's finally safe to drop the lock. We've pushed the bytes we indicated we pushed by adding to our mutex-guarded writer's buffer.
                    // Above dropping gives the reader some time to grab the lock when it arrives
                    std::hint::spin_loop(); // Give the reader a bit of extra time to grab the lock instead of *immediately* grabbing it again

                    saved_buf_lock = self.rb.saved_buf.spin_lock().unwrap(); // Grab lock without futex (hopefully faster)
                };
                bytes_written += num_copied;

                if cfg!(feature = "debug-print") {
                    std::println!("[RINGBUF DEBUG] Writer stopped waiting and found the reader copied {num_copied} bytes from our buffer");
                }

                if bytes_written == elems.len() {
                    return bytes_written;
                }
            }
        }

        if cfg!(feature = "debug-print") {
            std::println!(
                "[RINGBUF DEBUG] Writer is about to return after successfully single-copying {bytes_written} bytes and pushing the rest"
            );
        }

        bytes_written
            + unsafe {
                self.push_copy(&*(&elems[bytes_written..] as *const [T] as *const [MaybeUninit<T>]))
            }
    }
}

#[cfg(feature = "std")]
impl Producer<u8> {
    /// Reads at most `count` bytes
    /// from [`Read`](https://doc.rust-lang.org/std/io/trait.Read.html) instance
    /// and appends them to the ring buffer.
    /// If `count` is `None` then as much as possible bytes will be read.
    ///
    /// Returns `Ok(n)` if `read` is succeded. `n` is number of bytes been read.
    /// `n == 0` means that either `read` returned zero or ring buffer is full.
    ///
    /// If `read` is failed or returned an invalid number then error is returned.
    pub fn read_from(&mut self, reader: &mut dyn Read, count: Option<usize>) -> io::Result<usize> {
        let mut err = None;
        let n = unsafe {
            self.push_access(|left, _| -> usize {
                let left = match count {
                    Some(c) => {
                        if c < left.len() {
                            &mut left[0..c]
                        } else {
                            left
                        }
                    }
                    None => left,
                };
                match reader
                    .read(&mut *(left as *mut [MaybeUninit<u8>] as *mut [u8]))
                    .and_then(|n| {
                        if n <= left.len() {
                            Ok(n)
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "Read operation returned an invalid number",
                            ))
                        }
                    }) {
                    Ok(n) => n,
                    Err(e) => {
                        err = Some(e);
                        0
                    }
                }
            })
        };
        match err {
            Some(e) => Err(e),
            None => Ok(n),
        }
    }
}

#[cfg(feature = "std")]
impl Write for Producer<u8> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            let n = self.push_slice(buffer);
            if n == 0 && self.is_consumer_alive() {
                if !self.nonblocking {
                    continue;
                } else {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
            }
            /*
            else if n == 0 && !self.is_consumer_alive() {
                return Err(io::ErrorKind::NotFound.into());
            }
            */
            else {
                return Ok(n);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
