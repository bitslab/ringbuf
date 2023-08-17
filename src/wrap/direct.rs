use super::frozen::Frozen;
use crate::{
    consumer::PopIter,
    rb::traits::{RbRef, ToRbRef},
    traits::{consumer::Consumer, producer::Producer, Observer},
};
use core::{fmt, mem::MaybeUninit, num::NonZeroUsize};
#[cfg(feature = "std")]
use std::io;

pub struct Direct<R: RbRef, const P: bool, const C: bool> {
    rb: R,
}

/// Observer of ring buffer.
pub type Obs<R> = Direct<R, false, false>;
/// Producer of ring buffer.
pub type Prod<R> = Direct<R, true, false>;
/// Consumer of ring buffer.
pub type Cons<R> = Direct<R, false, true>;

impl<R: RbRef> Clone for Obs<R> {
    fn clone(&self) -> Self {
        Self { rb: self.rb.clone() }
    }
}

impl<R: RbRef, const P: bool, const C: bool> Direct<R, P, C> {
    /// # Safety
    ///
    /// There must be no more than one wrapper with the same parameter being `true`.
    pub unsafe fn new(rb: R) -> Self {
        Self { rb }
    }

    pub fn observe(&self) -> Obs<R> {
        Obs { rb: self.rb.clone() }
    }

    pub fn freeze(self) -> Frozen<R, P, C> {
        unsafe { Frozen::new(self.rb) }
    }
}

impl<R: RbRef, const P: bool, const C: bool> ToRbRef for Direct<R, P, C> {
    type RbRef = R;
    fn rb_ref(&self) -> &R {
        &self.rb
    }
    fn into_rb_ref(self) -> R {
        self.rb
    }
}

impl<R: RbRef, const P: bool, const C: bool> AsRef<Self> for Direct<R, P, C> {
    fn as_ref(&self) -> &Self {
        self
    }
}
impl<R: RbRef, const P: bool, const C: bool> AsMut<Self> for Direct<R, P, C> {
    fn as_mut(&mut self) -> &mut Self {
        self
    }
}

impl<R: RbRef, const P: bool, const C: bool> Observer for Direct<R, P, C> {
    type Item = <R::Target as Observer>::Item;

    #[inline]
    fn capacity(&self) -> NonZeroUsize {
        self.rb().capacity()
    }
    #[inline]
    fn read_index(&self) -> usize {
        self.rb().read_index()
    }
    #[inline]
    fn write_index(&self) -> usize {
        self.rb().write_index()
    }
    #[inline]
    unsafe fn unsafe_slices(&self, start: usize, end: usize) -> (&mut [MaybeUninit<Self::Item>], &mut [MaybeUninit<Self::Item>]) {
        self.rb().unsafe_slices(start, end)
    }
}

impl<R: RbRef> Producer for Prod<R> {
    #[inline]
    unsafe fn set_write_index(&self, value: usize) {
        self.rb().set_write_index(value)
    }
}

impl<R: RbRef> Consumer for Cons<R> {
    #[inline]
    unsafe fn set_read_index(&self, value: usize) {
        self.rb().set_read_index(value)
    }
}

#[cfg(feature = "std")]
impl<R: RbRef> io::Write for Prod<R>
where
    Self: Producer<Item = u8>,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        <Self as Producer>::write(self, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl<R: RbRef> fmt::Write for Prod<R>
where
    Self: Producer<Item = u8>,
{
    fn write_str(&mut self, s: &str) -> fmt::Result {
        <Self as Producer>::write_str(self, s)
    }
}

impl<R: RbRef> IntoIterator for Cons<R> {
    type Item = <Self as Observer>::Item;
    type IntoIter = PopIter<Self, Self>;
    fn into_iter(self) -> Self::IntoIter {
        PopIter::new(self)
    }
}

#[cfg(feature = "std")]
impl<R: RbRef> io::Read for Cons<R>
where
    Self: Consumer<Item = u8>,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        <Self as Consumer>::read(self, buf)
    }
}
