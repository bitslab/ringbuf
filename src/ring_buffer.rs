use alloc::{sync::Arc, vec::Vec};
use cache_padded::CachePadded;
use core::{
    cell::UnsafeCell,
    cmp,
    convert::{AsMut, AsRef},
    marker::PhantomData,
    mem::MaybeUninit,
    ops::Deref,
    ptr, slice,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{consumer::Consumer, producer::Producer};

pub trait Container<T>: AsRef<[MaybeUninit<T>]> + AsMut<[MaybeUninit<T>]> {}
impl<T, C> Container<T> for C where C: AsRef<[MaybeUninit<T>]> + AsMut<[MaybeUninit<T>]> {}

struct Storage<T, C: Container<T>> {
    len: usize,
    container: UnsafeCell<C>,
    phantom: PhantomData<T>,
}

unsafe impl<T, C: Container<T>> Sync for Storage<T, C> {}

impl<T, C: Container<T>> Storage<T, C> {
    fn new(mut container: C) -> Self {
        Self {
            len: container.as_mut().len(),
            container: UnsafeCell::new(container),
            phantom: PhantomData,
        }
    }

    fn into_inner(self) -> C {
        self.container.into_inner()
    }

    fn len(&self) -> usize {
        self.len
    }

    unsafe fn as_slice(&self) -> &[MaybeUninit<T>] {
        (&*self.container.get()).as_ref()
    }

    #[warn(clippy::mut_from_ref)]
    unsafe fn as_mut_slice(&self) -> &mut [MaybeUninit<T>] {
        (&mut *self.container.get()).as_mut()
    }
}

pub trait RingBufferRef<T, C: Container<T>>: Deref<Target = RingBuffer<T, C>> {}
impl<T, C: Container<T>> RingBufferRef<T, C> for Arc<RingBuffer<T, C>> {}
impl<'a, T, C: Container<T>> RingBufferRef<T, C> for &'a RingBuffer<T, C> {}

pub struct RingBuffer<T, C: Container<T>> {
    data: Storage<T, C>,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
}

impl<T, C: Container<T>> RingBuffer<T, C> {
    fn head(&self) -> usize {
        self.head.load(Ordering::Acquire)
    }
    fn tail(&self) -> usize {
        self.head.load(Ordering::Acquire)
    }

    pub unsafe fn from_raw_parts(container: C, head: usize, tail: usize) -> Self {
        Self {
            data: Storage::new(container),
            head: CachePadded::new(AtomicUsize::new(head)),
            tail: CachePadded::new(AtomicUsize::new(tail)),
        }
    }

    /// Splits ring buffer into producer and consumer.
    pub fn split(self) -> (Producer<T, C, Arc<Self>>, Consumer<T, C, Arc<Self>>) {
        let arc = Arc::new(self);
        (Producer::new(arc.clone()), Consumer::new(arc))
    }

    pub fn split_ref(&mut self) -> (Producer<T, C, &Self>, Consumer<T, C, &Self>) {
        (Producer::new(self), Consumer::new(self))
    }

    /// The capacity of the ring buffer.
    ///
    /// This value does not change.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }
    fn modulus(&self) -> usize {
        2 * self.capacity()
    }

    /// The number of elements stored in the buffer at the moment.
    pub fn occupied_len(&self) -> usize {
        (self.modulus() + self.tail() - self.head()) % self.modulus()
    }
    /// The number of vacant places in the buffer at the moment.
    pub fn vacant_len(&self) -> usize {
        (self.modulus() + self.head() - self.tail() - self.capacity()) % self.modulus()
    }

    /// Checks if the ring buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.occupied_len() == 0
    }

    /// Checks if the ring buffer is full.
    pub fn is_full(&self) -> bool {
        self.vacant_len() == 0
    }

    /// Move ring buffer **head** pointer by `count` elements forward.
    ///
    /// # Safety
    ///
    /// First `count` elements in occupied area must be initialized before this call.
    ///
    /// Does not check if `count` is greater than number of elements in the ring buffer.*
    ///
    /// Allowed to call only from **consumer** side.
    pub unsafe fn move_head(&self, count: usize) {
        debug_assert!(count <= self.occupied_len());
        self.head
            .store((self.head() + count) % self.modulus(), Ordering::Release);
    }

    /// Move ring buffer **tail** pointer by `count` elements forward.
    ///
    /// # Safety
    ///
    /// First `count` elements in vacant area must be deinitialized (dropped) before this call.
    ///
    /// Does not check if `count` is greater than number of vacant places in the ring buffer.
    ///
    /// Allowed to call only from **producer** side.
    pub unsafe fn move_tail(&self, count: usize) {
        debug_assert!(count <= self.vacant_len());
        self.tail
            .store((self.tail() + count) % self.modulus(), Ordering::Release);
    }

    /// Returns a pair of slices which contain, in order, the occupied cells in the ring buffer.
    ///
    /// All elements in slices are guaranteed to be *initialized*.
    ///
    /// *The slices may not include elements pushed to the buffer by the concurring producer right after this call.*
    ///
    /// # Safety
    ///
    /// Allowed to call only from **consumer** side.
    pub unsafe fn occupied_slices(&self) -> (&mut [MaybeUninit<T>], &mut [MaybeUninit<T>]) {
        let head = self.head();
        let tail = self.tail();
        let len = self.data.len();

        let ranges = match head.cmp(&tail) {
            cmp::Ordering::Less => ((head % len)..(tail % len), 0..0),
            cmp::Ordering::Greater => ((head % len)..len, 0..(tail % len)),
            cmp::Ordering::Equal => (0..0, 0..0),
        };

        let ptr = self.data.as_mut_slice().as_mut_ptr();
        (
            slice::from_raw_parts_mut(ptr.add(ranges.0.start), ranges.0.len()),
            slice::from_raw_parts_mut(ptr.add(ranges.1.start), ranges.1.len()),
        )
    }

    /// Returns a pair of slices which contain, in order, the vacant cells in the ring buffer.
    ///
    /// All elements in slices are guaranteed to be *un-initialized*.
    ///
    /// *The slices may not include cells freed by the concurring consumer right after this call.*
    ///
    /// # Safety
    ///
    /// Allowed to call only from **producer** side.
    pub unsafe fn vacant_slices(&self) -> (&mut [MaybeUninit<T>], &mut [MaybeUninit<T>]) {
        let head = self.head();
        let tail = self.tail();
        let len = self.data.len();

        let ranges = match head.cmp(&tail) {
            cmp::Ordering::Less => ((tail % len)..len, 0..(head % len)),
            cmp::Ordering::Greater => ((tail % len)..(head % len), 0..0),
            cmp::Ordering::Equal => (0..0, 0..0),
        };

        let ptr = self.data.as_mut_slice().as_mut_ptr();
        (
            slice::from_raw_parts_mut(ptr.add(ranges.0.start), ranges.0.len()),
            slice::from_raw_parts_mut(ptr.add(ranges.1.start), ranges.1.len()),
        )
    }

    /// Removes exactly `count` elements from the head of ring buffer and drops them.
    ///
    /// *Panics if `count` is greater than number of elements stored in the buffer.*
    pub fn skip(&self, count: usize) {
        let (left, right) = unsafe { self.occupied_slices() };
        assert!(count <= left.len() + right.len());
        for elem in left.iter_mut().chain(right.iter_mut()).take(count) {
            unsafe { ptr::drop_in_place(elem.as_mut_ptr()) };
        }
    }
}

impl<T, C: Container<T>> Drop for RingBuffer<T, C> {
    fn drop(&mut self) {
        self.skip(self.occupied_len());
    }
}

pub type VecRingBuffer<T> = RingBuffer<T, Vec<MaybeUninit<T>>>;
pub type StaticRingBuffer<T, const N: usize> = RingBuffer<T, [MaybeUninit<T>; N]>;

impl<T> VecRingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        let mut data = Vec::new();
        data.resize_with(capacity, MaybeUninit::uninit);
        unsafe { Self::from_raw_parts(data, 0, 0) }
    }
}

impl<T, const N: usize> Default for StaticRingBuffer<T, N> {
    fn default() -> Self {
        let uninit = MaybeUninit::<[T; N]>::uninit();
        let array = unsafe { (&uninit as *const _ as *const [MaybeUninit<T>; N]).read() };
        unsafe { Self::from_raw_parts(array, 0, 0) }
    }
}

/*
/// Moves at most `count` items from the `src` consumer to the `dst` producer.
/// Consumer and producer may be of different buffers as well as of the same one.
///
/// `count` is the number of items being moved, if `None` - as much as possible items will be moved.
///
/// Returns number of items been moved.
pub fn move_items<T>(src: &mut Consumer<T>, dst: &mut Producer<T>, count: Option<usize>) -> usize {
    unsafe {
        src.pop_access(|src_left, src_right| -> usize {
            dst.push_access(|dst_left, dst_right| -> usize {
                let n = count.unwrap_or_else(|| {
                    min(
                        src_left.len() + src_right.len(),
                        dst_left.len() + dst_right.len(),
                    )
                });
                let mut m = 0;
                let mut src = (SlicePtr::new(src_left), SlicePtr::new(src_right));
                let mut dst = (SlicePtr::new(dst_left), SlicePtr::new(dst_right));

                loop {
                    let k = min(n - m, min(src.0.len, dst.0.len));
                    if k == 0 {
                        break;
                    }
                    copy(src.0.ptr, dst.0.ptr, k);
                    if src.0.len == k {
                        src.0 = src.1;
                        src.1 = SlicePtr::null();
                    } else {
                        src.0.shift(k);
                    }
                    if dst.0.len == k {
                        dst.0 = dst.1;
                        dst.1 = SlicePtr::null();
                    } else {
                        dst.0.shift(k);
                    }
                    m += k
                }

                m
            })
        })
    }
}
*/
