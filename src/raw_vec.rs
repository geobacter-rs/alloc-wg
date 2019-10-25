use crate::{
    alloc::{
        AbortAlloc,
        AllocRef,
        BuildAllocRef,
        CapacityOverflow,
        DeallocRef,
        Global,
        Layout,
        NonZeroLayout,
        ReallocRef,
    },
    boxed::Box,
    collections::CollectionAllocErr::{self},
};
use core::{cmp, marker::PhantomData, mem, ptr, ptr::NonNull, slice};
use std::convert::TryInto;

/// A low-level utility for more ergonomically allocating, reallocating, and deallocating
/// a buffer of memory on the heap without having to worry about all the corner cases
/// involved. This type is excellent for building your own data structures like Vec and `VecDeque`.
/// In particular:
///
/// * Produces `Unique::empty()` on zero-sized types
/// * Produces `Unique::empty()` on zero-length allocations
/// * Catches all overflows in capacity computations (promotes them to "capacity overflow" panics)
/// * Guards against 32-bit systems allocating more than `isize::MAX` bytes
/// * Guards against overflowing your length
/// * Aborts on OOM or calls `handle_alloc_error` as applicable
/// * Avoids freeing `Unique::empty()`
/// * Contains a `ptr::Unique` and thus endows the user with all related benefits
///
/// This type does not in anyway inspect the memory that it manages. When dropped it *will*
/// free its memory, but it *won't* try to Drop its contents. It is up to the user of `RawVec`
/// to handle the actual things *stored* inside of a `RawVec`.
///
/// Note that a `RawVec` always forces its capacity to be `usize::MAX` for zero-sized types.
/// This enables you to use capacity growing logic catch the overflows in your length
/// that might occur with zero-sized types.
///
/// However this means that you need to be careful when round-tripping this type
/// with a `Box<[T]>`: `capacity()` won't yield the len. However `with_capacity`,
/// `shrink_to_fit`, and `from_box` will actually set `RawVec`'s private capacity
/// field. This allows zero-sized types to not be special-cased by consumers of
/// this type.
// Using `NonNull` + `PhantomData` instead of `Unique` to stay on stable as long as possible
pub struct RawVec<T, B: BuildAllocRef = AbortAlloc<Global>> {
    ptr: NonNull<T>,
    capacity: usize,
    build_alloc: B,
    _owned: PhantomData<T>,
}

impl<T> RawVec<T> {
    /// HACK(Centril): This exists because `#[unstable]` `const fn`s needn't conform
    /// to `min_const_fn` and so they cannot be called in `min_const_fn`s either.
    ///
    /// If you change `RawVec<T>::new` or dependencies, please take care to not
    /// introduce anything that would truly violate `min_const_fn`.
    ///
    /// NOTE: We could avoid this hack and check conformance with some
    /// `#[rustc_force_min_const_fn]` attribute which requires conformance
    /// with `min_const_fn` but does not necessarily allow calling it in
    /// `stable(...) const fn` / user code not enabling `foo` when
    /// `#[rustc_const_unstable(feature = "foo", ..)]` is present.
    pub const NEW: Self = Self::new();

    /// Creates the biggest possible `RawVec` (on the system heap)
    /// without allocating. If `T` has positive size, then this makes a
    /// `RawVec` with capacity `0`. If `T` is zero-sized, then it makes a
    /// `RawVec` with capacity `usize::MAX`. Useful for implementing
    /// delayed allocation.
    pub const fn new() -> Self {
        // FIXME(Centril): Reintegrate this with `fn new_in` when we can.

        // `!0` is `usize::MAX`. This branch should be stripped at compile time.
        // FIXME(mark-i-m): use this line when `if`s are allowed in `const`:
        // let cap = if mem::size_of::<T>() == 0 { !0 } else { 0 };

        // `Unique::empty()` doubles as "unallocated" and "zero-sized allocation".
        Self {
            ptr: NonNull::dangling(),
            // FIXME(mark-i-m): use `cap` when ifs are allowed in const
            capacity: [0, !0][(mem::size_of::<T>() == 0) as usize],
            build_alloc: AbortAlloc(Global),
            _owned: PhantomData,
        }
    }

    /// Creates a `RawVec` (on the system heap) with exactly the
    /// capacity and alignment requirements for a `[T; capacity]`. This is
    /// equivalent to calling `RawVec::new` when `capacity` is `0` or `T` is
    /// zero-sized. Note that if `T` is zero-sized this means you will
    /// *not* get a `RawVec` with the requested capacity.
    ///
    /// # Panics
    ///
    /// * if the requested capacity exceeds `usize::MAX` bytes.
    /// * on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    ///
    /// # Aborts
    ///
    /// * on OOM
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_in(capacity, AbortAlloc(Global))
    }

    /// Like `with_capacity`, but guarantees the buffer is zeroed.
    #[inline]
    pub fn with_capacity_zeroed(capacity: usize) -> Self {
        Self::with_capacity_zeroed_in(capacity, AbortAlloc(Global))
    }

    /// Reconstitutes a RawVec from a pointer, and capacity.
    ///
    /// # Safety
    ///
    /// The ptr must be allocated (via the default allocator `Global`), and with the
    /// given capacity. The capacity cannot exceed `isize::MAX` (only a concern on 32-bit systems).
    /// If the ptr and capacity come from a RawVec created with `Global`, then this is guaranteed.
    #[inline]
    pub unsafe fn from_raw_parts(ptr: *mut T, capacity: usize) -> Self {
        Self::from_raw_parts_in(ptr, capacity, AbortAlloc(Global))
    }
}

impl<T, B: BuildAllocRef> RawVec<T, B> {
    /// Like `new` but parameterized over the choice of allocator for the returned `RawVec`.
    pub fn new_in(mut a: B::Ref) -> Self {
        let capacity = if mem::size_of::<T>() == 0 { !0 } else { 0 };
        Self {
            ptr: NonNull::dangling(),
            capacity,
            build_alloc: a.get_build_alloc(),
            _owned: PhantomData,
        }
    }

    #[inline]
    /// Like `with_capacity` but parameterized over the choice of allocator for the returned
    /// `RawVec`.
    ///
    /// # Panics
    ///
    /// * `CapacityOverflow` if the requested capacity exceeds `usize::MAX` bytes.
    /// * `CapacityOverflow` on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    pub fn with_capacity_in(capacity: usize, a: B::Ref) -> Self
    where
        B::Ref: AllocRef<Error = crate::Never>,
    {
        match Self::try_with_capacity_in(capacity, a) {
            Ok(vec) => vec,
            Err(CollectionAllocErr::CapacityOverflow) => capacity_overflow(),
            Err(CollectionAllocErr::AllocError { .. }) => unreachable!("Infallible allocation"),
        }
    }

    #[inline]
    /// Like `with_capacity` but parameterized over the choice of allocator for the returned
    /// `RawVec`.
    ///
    /// # Errors
    ///
    /// * `CapacityOverflow` if the requested capacity exceeds `usize::MAX` bytes.
    /// * `CapacityOverflow` on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    /// * `AllocError` on OOM
    pub fn try_with_capacity_in(capacity: usize, a: B::Ref) -> Result<Self, CollectionAllocErr<B>>
    where
        B::Ref: AllocRef,
    {
        Self::allocate_in(capacity, false, a)
    }

    #[inline]
    /// Like `with_capacity_zeroed` but parameterized over the choice of allocator for the returned
    /// `RawVec`.
    ///
    /// # Panics
    ///
    /// * `CapacityOverflow` if the requested capacity exceeds `usize::MAX` bytes.
    /// * `CapacityOverflow` on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    pub fn with_capacity_zeroed_in(capacity: usize, a: B::Ref) -> Self
    where
        B::Ref: AllocRef<Error = crate::Never>,
    {
        match Self::try_with_capacity_zeroed_in(capacity, a) {
            Ok(vec) => vec,
            Err(CollectionAllocErr::CapacityOverflow) => capacity_overflow(),
            Err(CollectionAllocErr::AllocError { .. }) => unreachable!("Infallible allocation"),
        }
    }

    #[inline]
    /// Like `with_capacity_zeroed` but parameterized over the choice of allocator for the returned
    /// `RawVec`.
    ///
    /// # Errors
    ///
    /// * `CapacityOverflow` if the requested capacity exceeds `usize::MAX` bytes.
    /// * `CapacityOverflow` on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    /// * `AllocError` on OOM
    pub fn try_with_capacity_zeroed_in(
        capacity: usize,
        a: B::Ref,
    ) -> Result<Self, CollectionAllocErr<B>>
    where
        B::Ref: AllocRef,
    {
        Self::allocate_in(capacity, true, a)
    }

    fn allocate_in(
        capacity: usize,
        zeroed: bool,
        mut alloc: B::Ref,
    ) -> Result<Self, CollectionAllocErr<B>>
    where
        B::Ref: AllocRef,
    {
        let elem_size = mem::size_of::<T>();

        let alloc_size = capacity
            .checked_mul(elem_size)
            .ok_or(CollectionAllocErr::CapacityOverflow)?;
        alloc_guard(alloc_size)?;

        // handles ZSTs and `capacity = 0` alike
        let ptr = if alloc_size == 0 {
            NonNull::<T>::dangling()
        } else {
            let layout = unsafe {
                NonZeroLayout::from_size_align_unchecked(alloc_size, mem::align_of::<T>())
            };
            let result = if zeroed {
                alloc.alloc_zeroed(layout)
            } else {
                alloc.alloc(layout)
            };
            result
                .map_err(|inner| CollectionAllocErr::AllocError { layout, inner })?
                .cast()
        };

        unsafe {
            Ok(Self::from_raw_parts_in(
                ptr.as_ptr(),
                capacity,
                alloc.get_build_alloc(),
            ))
        }
    }

    /// Reconstitutes a `RawVec` from a pointer, capacity, and allocator.
    ///
    /// # Safety
    ///
    /// The ptr must be allocated via `build_alloc`, and with the given capacity. The capacity
    /// cannot exceed `isize::MAX` (only a concern on 32-bit systems). If the ptr and capacity come
    /// from a `RawVec` created via `build_alloc`, then this is guaranteed.
    pub unsafe fn from_raw_parts_in(ptr: *mut T, capacity: usize, build_alloc: B) -> Self {
        Self {
            ptr: NonNull::new_unchecked(ptr),
            capacity,
            build_alloc,
            _owned: PhantomData,
        }
    }

    /// Gets a raw pointer to the start of the allocation. Note that this is
    /// `Unique::empty()` if `capacity = 0` or `T` is zero-sized. In the former case, you must
    /// be careful.
    pub fn ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Gets the capacity of the allocation.
    ///
    /// This will always be `usize::MAX` if `T` is zero-sized.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        if mem::size_of::<T>() == 0 {
            !0
        } else {
            self.capacity
        }
    }

    /// Returns a shared reference to the allocator builder backing this `RawVec`.
    pub fn build_alloc(&self) -> &B {
        &self.build_alloc
    }

    /// Returns a mutable reference to the allocator builder backing this `RawVec`.
    pub fn build_alloc_mut(&mut self) -> &mut B {
        &mut self.build_alloc
    }

    /// Returns the allocator used by this `RawVec` and the used layout, if any.
    /// The layout is `None` if the capacity of this `RawVec` is `0` or if `T` is a zero sized type.
    pub fn alloc_ref(&mut self) -> (B::Ref, Option<NonZeroLayout>) {
        let size = mem::size_of::<T>() * self.capacity;
        unsafe {
            let layout = Layout::from_size_align_unchecked(size, mem::align_of::<T>())
                .try_into()
                .ok();
            let ptr = self.ptr.cast();
            let alloc = self.build_alloc_mut().build_alloc_ref(ptr, layout);
            (alloc, layout)
        }
    }

    /// Converts the entire buffer into `Box<[mem::MaybeUninit<T>]>`.
    ///
    /// Note that this will correctly reconstitute any `cap` changes
    /// that may have been performed. (see description of type for details)
    pub fn into_box(self) -> Box<[mem::MaybeUninit<T>], B> {
        let ptr = self.ptr() as *mut mem::MaybeUninit<T>;
        unsafe {
            let slice = slice::from_raw_parts_mut(ptr, self.capacity);
            let builder = ptr::read(&self.build_alloc);
            let output = Box::from_raw_in(slice, builder);
            mem::forget(self);
            output
        }
    }

    /// Calculates the buffer's new size given that it'll hold `used_capacity +
    /// needed_extra_capacity` elements. This logic is used in amortized reserve methods.
    /// Returns `(new_capacity, new_alloc_size)`.
    fn amortized_new_size(
        &self,
        used_capacity: usize,
        needed_extra_capacity: usize,
    ) -> Result<usize, CapacityOverflow> {
        // Nothing we can really do about these checks :(
        let required_cap = used_capacity
            .checked_add(needed_extra_capacity)
            .ok_or(CapacityOverflow)?;
        // Cannot overflow, because `cap <= isize::MAX`, and type of `cap` is `usize`.
        let double_cap = self.capacity * 2;
        // `double_cap` guarantees exponential growth.
        Ok(cmp::max(double_cap, required_cap))
    }

    /// Doubles the size of the type's backing allocation. This is common enough
    /// to want to do that it's easiest to just have a dedicated method. Slightly
    /// more efficient logic can be provided for this than the general case.
    ///
    /// This function is ideal for when pushing elements one-at-a-time because
    /// you don't need to incur the costs of the more general computations
    /// reserve needs to do to guard against overflow. You do however need to
    /// manually check if your `len == capacity`.
    ///
    /// # Panics
    ///
    /// * Panics if `T` is zero-sized on the assumption that you managed to exhaust
    ///   all `usize::MAX` slots in your imaginary buffer.
    /// * Panics on 32-bit platforms if the requested capacity exceeds
    ///   `isize::MAX` bytes.
    ///
    /// # Aborts
    ///
    /// Aborts on OOM
    ///
    /// # Examples
    ///
    /// ```
    /// # use core::ptr;
    /// # use alloc_wg::raw_vec::RawVec;
    /// struct MyVec<T> {
    ///     buf: RawVec<T>,
    ///     len: usize,
    /// }
    ///
    /// impl<T> MyVec<T> {
    ///     pub fn push(&mut self, elem: T) {
    ///         if self.len == self.buf.capacity() {
    ///             self.buf.double();
    ///         }
    ///         // double would have aborted or panicked if the len exceeded
    ///         // `isize::MAX` so this is safe to do unchecked now.
    ///         unsafe {
    ///             ptr::write(self.buf.ptr().add(self.len), elem);
    ///         }
    ///         self.len += 1;
    ///     }
    /// }
    /// # fn main() {
    /// #   let mut vec = MyVec { buf: RawVec::new(), len: 0 };
    /// #   vec.push(1);
    /// # }
    /// ```
    pub fn double(&mut self)
    where
        B::Ref: ReallocRef<Error = crate::Never>,
    {
        match self.try_double() {
            Ok(_) => (),
            Err(CollectionAllocErr::CapacityOverflow) => capacity_overflow(),
            Err(CollectionAllocErr::AllocError { .. }) => unreachable!("Infallible allocation"),
        }
    }

    /// The same as `double`, but returns on errors instead of panicking.
    #[inline(never)]
    #[cold]
    pub fn try_double(&mut self) -> Result<(), CollectionAllocErr<B>>
    where
        B::Ref: ReallocRef,
    {
        unsafe {
            let elem_size = mem::size_of::<T>();

            // Since we set the capacity to `usize::MAX` when `elem_size` is
            // 0, getting to here necessarily means the `RawVec` is overfull.
            if elem_size == 0 {
                return Err(CollectionAllocErr::CapacityOverflow);
            }

            let (mut alloc, old_layout) = self.alloc_ref();
            let (new_cap, ptr) = if let Some(layout) = old_layout {
                // Since we guarantee that we never allocate more than
                // `isize::MAX` bytes, `elem_size * self.cap <= isize::MAX` as
                // a precondition, so this can't overflow. Additionally the
                // alignment will never be too large as to "not be
                // satisfiable", so `Layout::from_size_align` will always
                // return `Some`.
                //
                // TL;DR, we bypass runtime checks due to dynamic assertions
                // in this module, allowing us to use
                // `from_size_align_unchecked`.
                let new_cap = 2 * self.capacity;
                let new_size = new_cap * elem_size;
                alloc_guard(new_size)?;
                let ptr = alloc
                    .realloc(self.ptr.cast(), layout, new_size)
                    .map_err(|inner| CollectionAllocErr::AllocError {
                        inner,
                        layout: NonZeroLayout::from_size_align_unchecked(new_size, layout.align()),
                    })?;
                (new_cap, ptr.cast())
            } else {
                // Skip to 4 because tiny `Vec`'s are dumb; but not if that
                // would cause overflow.
                let new_cap = if elem_size > (!0) / 8 { 1 } else { 4 };
                let new_layout = NonZeroLayout::array::<T>(new_cap)?;
                let ptr = alloc
                    .alloc(NonZeroLayout::array::<T>(new_cap)?)
                    .map_err(|inner| CollectionAllocErr::AllocError {
                        inner,
                        layout: new_layout,
                    })?;
                (new_cap, ptr.cast())
            };

            self.ptr = ptr;
            self.capacity = new_cap;
            Ok(())
        }
    }

    /// Attempts to double the size of the type's backing allocation in place. This is common
    /// enough to want to do that it's easiest to just have a dedicated method. Slightly
    /// more efficient logic can be provided for this than the general case.
    ///
    /// Returns `true` if the reallocation attempt has succeeded.
    ///
    /// # Panics
    ///
    /// * Panics if `T` is zero-sized on the assumption that you managed to exhaust
    ///   all `usize::MAX` slots in your imaginary buffer.
    /// * Panics on 32-bit platforms if the requested capacity exceeds
    ///   `isize::MAX` bytes.
    pub fn double_in_place(&mut self) -> bool
    where
        B::Ref: AllocRef,
    {
        if let Ok(success) = self.try_double_in_place() {
            success
        } else {
            capacity_overflow()
        }
    }

    /// The same as `double_in_place`, but returns on errors instead of panicking.
    #[inline(never)]
    #[cold]
    pub fn try_double_in_place(&mut self) -> Result<bool, CapacityOverflow>
    where
        B::Ref: AllocRef,
    {
        unsafe {
            let elem_size = mem::size_of::<T>();

            // Since we set the capacity to `usize::MAX` when `elem_size` is
            // 0, getting to here necessarily means the `RawVec` is overfull.
            if elem_size == 0 {
                return Err(CapacityOverflow);
            }

            let (mut alloc, old_layout) = if let (alloc, Some(layout)) = self.alloc_ref() {
                (alloc, layout)
            } else {
                return Ok(false); // nothing to double
            };

            // Since we guarantee that we never allocate more than `isize::MAX`
            // bytes, `elem_size * self.cap <= isize::MAX` as a precondition, so
            // this can't overflow.
            //
            // Similarly to with `double` above, we can go straight to
            // `Layout::from_size_align_unchecked` as we know this won't
            // overflow and the alignment is sufficiently small.
            let new_cap = 2 * self.capacity;
            let new_size = new_cap * elem_size;
            alloc_guard(new_size)?;
            Ok(alloc.grow_in_place(self.ptr.cast(), old_layout, new_size))
        }
    }

    /// Ensures that the buffer contains at least enough space to hold
    /// `used_capacity + needed_extra_capacity` elements. If it doesn't already have enough
    /// capacity, will reallocate enough space plus comfortable slack space to get amortized `O(1)`
    /// behavior. Will limit this behavior if it would needlessly cause itself to panic.
    ///
    /// If `used_capacity` exceeds `self.capacity()`, this may fail to actually allocate the
    /// requested space. This is not really unsafe, but the unsafe code *you* write that relies on
    /// the behavior of this function may break.
    ///
    /// This is ideal for implementing a bulk-push operation like `extend`.
    ///
    /// # Panics
    ///
    /// * if the requested capacity exceeds `usize::MAX` bytes.
    /// * on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use alloc_wg::raw_vec::RawVec;
    /// # use core::ptr;
    /// struct MyVec<T> {
    ///     buf: RawVec<T>,
    ///     len: usize,
    /// }
    ///
    /// impl<T: Clone> MyVec<T> {
    ///     pub fn push_all(&mut self, elems: &[T]) {
    ///         self.buf.reserve(self.len, elems.len());
    ///         // reserve would have aborted or panicked if the len exceeded
    ///         // `isize::MAX` so this is safe to do unchecked now.
    ///         for x in elems {
    ///             unsafe {
    ///                 ptr::write(self.buf.ptr().add(self.len), x.clone());
    ///             }
    ///             self.len += 1;
    ///         }
    ///     }
    /// }
    /// # fn main() {
    /// #   let mut vector = MyVec { buf: RawVec::new(), len: 0 };
    /// #   vector.push_all(&[1, 3, 5, 7, 9]);
    /// # }
    /// ```
    pub fn reserve(&mut self, used_capacity: usize, needed_extra_capacity: usize)
    where
        B::Ref: ReallocRef<Error = crate::Never>,
    {
        match self.try_reserve(used_capacity, needed_extra_capacity) {
            Ok(vec) => vec,
            Err(CollectionAllocErr::CapacityOverflow) => capacity_overflow(),
            Err(CollectionAllocErr::AllocError { .. }) => unreachable!("Infallible allocation"),
        }
    }

    /// The same as `reserve`, but returns on errors instead of panicking.
    pub fn try_reserve(
        &mut self,
        used_capacity: usize,
        needed_extra_capacity: usize,
    ) -> Result<(), CollectionAllocErr<B>>
    where
        B::Ref: ReallocRef,
    {
        self.reserve_internal(
            used_capacity,
            needed_extra_capacity,
            ReserveStrategy::Amortized,
        )
    }

    /// Ensures that the buffer contains at least enough space to hold
    /// `used_capacity + needed_extra_capacity` elements. If it doesn't already have
    /// enough capacity, will reallocate in place enough space plus comfortable slack
    /// space to get amortized `O(1)` behavior. Will limit this behaviour
    /// if it would needlessly cause itself to panic.
    ///
    /// If `used_capacity` exceeds `self.capacity()`, this may fail to actually allocate
    /// the requested space. This is not really unsafe, but the unsafe
    /// code *you* write that relies on the behavior of this function may break.
    ///
    /// Returns `true` if the reallocation attempt has succeeded.
    ///
    /// # Panics
    ///
    /// * if the requested capacity exceeds `usize::MAX` bytes.
    /// * on 32-bit platforms if the requested capacity exceeds `isize::MAX` bytes.
    pub fn reserve_exact(&mut self, used_capacity: usize, needed_extra_capacity: usize)
    where
        B::Ref: ReallocRef<Error = crate::Never>,
    {
        match self.try_reserve_exact(used_capacity, needed_extra_capacity) {
            Ok(_) => (),
            Err(CollectionAllocErr::CapacityOverflow) => capacity_overflow(),
            Err(CollectionAllocErr::AllocError { .. }) => unreachable!(),
        }
    }

    /// The same as `reserve_exact`, but returns on errors instead of panicking.
    pub fn try_reserve_exact(
        &mut self,
        used_capacity: usize,
        needed_extra_capacity: usize,
    ) -> Result<(), CollectionAllocErr<B>>
    where
        B::Ref: ReallocRef,
    {
        self.reserve_internal(used_capacity, needed_extra_capacity, ReserveStrategy::Exact)
    }

    /// Attempts to ensure that the buffer contains at least enough space to hold
    /// `used_capacity + needed_extra_capacity` elements. If it doesn't already have
    /// enough capacity, will reallocate in place enough space plus comfortable slack
    /// space to get amortized `O(1)` behavior. Will limit this behaviour
    /// if it would needlessly cause itself to panic.
    ///
    /// If `used_capacity` exceeds `self.capacity()`, this may fail to actually allocate
    /// the requested space. This is not really unsafe, but the unsafe
    /// code *you* write that relies on the behavior of this function may break.
    ///
    /// Returns `true` if the reallocation attempt has succeeded.
    ///
    /// # Panics
    ///
    /// * Panics if the requested capacity exceeds `usize::MAX` bytes.
    /// * Panics on 32-bit platforms if the requested capacity exceeds
    ///   `isize::MAX` bytes.
    pub fn reserve_in_place(&mut self, used_capacity: usize, needed_extra_capacity: usize) -> bool
    where
        B::Ref: AllocRef,
    {
        if let Ok(success) = self.try_reserve_in_place(used_capacity, needed_extra_capacity) {
            success
        } else {
            capacity_overflow()
        }
    }

    /// The same as `reserve_in_place`, but returns on errors instead of panicking.
    pub fn try_reserve_in_place(
        &mut self,
        used_capacity: usize,
        needed_extra_capacity: usize,
    ) -> Result<bool, CapacityOverflow>
    where
        B::Ref: AllocRef,
    {
        unsafe {
            // NOTE: we don't early branch on ZSTs here because we want this
            // to actually catch "asking for more than usize::MAX" in that case.
            // If we make it past the first branch then we are guaranteed to
            // return.

            // Don't actually need any more capacity. If the current `cap` is 0, we can't
            // reallocate in place.
            // Wrapping in case they give a bad `used_capacity`
            if self.capacity().wrapping_sub(used_capacity) >= needed_extra_capacity {
                return Ok(false);
            }

            let (mut alloc, old_layout) = if let (alloc, Some(layout)) = self.alloc_ref() {
                (alloc, layout)
            } else {
                return Ok(false); // nothing to double
            };

            let new_cap = self.amortized_new_size(used_capacity, needed_extra_capacity)?;
            let new_size = new_cap * mem::size_of::<T>();

            // Here, `cap < used_capacity + needed_extra_capacity <= new_cap`
            // (regardless of whether `self.cap - used_capacity` wrapped).
            // Therefore, we can safely call `grow_in_place`.
            // FIXME: may crash and burn on over-reserve
            alloc_guard(new_size)?;
            if alloc.grow_in_place(self.ptr.cast(), old_layout, new_size) {
                self.capacity = new_cap;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    /// Shrinks the allocation down to the specified amount. If the given amount
    /// is 0, actually completely deallocates.
    ///
    /// # Panics
    ///
    /// Panics if the given amount is *larger* than the current capacity.
    pub fn shrink_to_fit(&mut self, amount: usize)
    where
        B::Ref: ReallocRef<Error = crate::Never>,
    {
        match self.try_shrink_to_fit(amount) {
            Ok(_) => (),
            Err(CollectionAllocErr::CapacityOverflow) => {
                panic!("Tried to shrink to a larger capacity")
            }
            Err(CollectionAllocErr::AllocError { .. }) => unreachable!(),
        }
    }

    /// The same as `shrink_to_fit`, but returns on errors instead of panicking.
    pub fn try_shrink_to_fit(&mut self, amount: usize) -> Result<(), CollectionAllocErr<B>>
    where
        B::Ref: ReallocRef,
    {
        let elem_size = mem::size_of::<T>();

        // Set the `cap` because they might be about to promote to a `Box<[T]>`
        if elem_size == 0 {
            self.capacity = amount;
            return Ok(());
        }

        // This check is my waterloo; it's the only thing Vec wouldn't have to do.
        if self.capacity < amount {
            return Err(CollectionAllocErr::CapacityOverflow);
        }

        if amount == 0 {
            // We want to create a new zero-length vector within the
            // same allocator.  We use ptr::write to avoid an
            // erroneous attempt to drop the contents, and we use
            // ptr::read to sidestep condition against destructuring
            // types that implement Drop.

            unsafe {
                let build_alloc = ptr::read(self.build_alloc());
                self.dealloc_buffer();
                ptr::write(
                    self,
                    Self::from_raw_parts_in(NonNull::dangling().as_ptr(), 0, build_alloc),
                );
            }
        } else if self.capacity != amount {
            unsafe {
                // We know here that our `amount` is greater than zero. This
                // implies, via the assert above, that capacity is also greater
                // than zero, which means that we've got a current layout that
                // "fits"
                //
                // We also know that `self.cap` is greater than `amount`, and
                // consequently we don't need runtime checks for creating either
                // layout
                let old_size = elem_size * self.capacity;
                let new_size = elem_size * amount;
                let align = mem::align_of::<T>();
                let old_layout = NonZeroLayout::from_size_align_unchecked(old_size, align);
                let ptr = self.ptr.cast();
                self.build_alloc
                    .build_alloc_ref(ptr, Some(old_layout))
                    .realloc(ptr, old_layout, new_size)
                    .map_err(|inner| CollectionAllocErr::AllocError {
                        layout: NonZeroLayout::from_size_align_unchecked(new_size, align),
                        inner,
                    })?;
            }
            self.capacity = amount;
        }
        Ok(())
    }

    fn reserve_internal(
        &mut self,
        used_capacity: usize,
        needed_extra_capacity: usize,
        strategy: ReserveStrategy,
    ) -> Result<(), CollectionAllocErr<B>>
    where
        B::Ref: ReallocRef,
    {
        unsafe {
            // NOTE: we don't early branch on ZSTs here because we want this
            // to actually catch "asking for more than usize::MAX" in that case.
            // If we make it past the first branch then we are guaranteed to
            // panic.

            // Don't actually need any more capacity.
            // Wrapping in case they gave a bad `used_capacity`.
            if self.capacity().wrapping_sub(used_capacity) >= needed_extra_capacity {
                return Ok(());
            }

            // Nothing we can really do about these checks :(
            let new_cap = match strategy {
                ReserveStrategy::Exact => used_capacity
                    .checked_add(needed_extra_capacity)
                    .ok_or(CollectionAllocErr::CapacityOverflow)?,
                ReserveStrategy::Amortized => {
                    self.amortized_new_size(used_capacity, needed_extra_capacity)?
                }
            };
            let new_layout = NonZeroLayout::array::<T>(new_cap)?;

            alloc_guard(new_layout.size())?;

            let (mut alloc, old_layout) = self.alloc_ref();
            let result = if let Some(layout) = old_layout {
                debug_assert_eq!(new_layout.align(), layout.align());
                alloc.realloc(self.ptr.cast(), layout, new_layout.size())
            } else {
                alloc.alloc(new_layout)
            };

            self.ptr = result
                .map_err(|inner| CollectionAllocErr::AllocError {
                    layout: new_layout,
                    inner,
                })?
                .cast();
            self.capacity = new_cap;

            Ok(())
        }
    }
}

impl<T, B: BuildAllocRef> From<Box<[T], B>> for RawVec<T, B> {
    fn from(slice: Box<[T], B>) -> Self {
        let len = slice.len();
        let (ptr, builder) = Box::into_raw_non_null_alloc(slice);
        Self {
            ptr: ptr.cast(),
            capacity: len,
            build_alloc: builder,
            _owned: PhantomData,
        }
    }
}

// Copy for passing by value without warnings
#[derive(Copy, Clone)]
enum ReserveStrategy {
    Exact,
    Amortized,
}

impl<T, B: BuildAllocRef> RawVec<T, B> {
    /// Frees the memory owned by the RawVec *without* trying to Drop its contents.
    pub unsafe fn dealloc_buffer(&mut self) {
        if let (mut alloc, Some(layout)) = self.alloc_ref() {
            alloc.dealloc(self.ptr.cast(), layout)
        }
    }
}

#[cfg(feature = "dropck_eyepatch")]
unsafe impl<#[may_dangle] T, B: BuildAllocRef> Drop for RawVec<T, B> {
    /// Frees the memory owned by the RawVec *without* trying to Drop its contents.
    fn drop(&mut self) {
        unsafe {
            self.dealloc_buffer();
        }
    }
}

#[cfg(not(feature = "dropck_eyepatch"))]
impl<T, B: BuildAllocRef> Drop for RawVec<T, B> {
    /// Frees the memory owned by the RawVec *without* trying to Drop its contents.
    fn drop(&mut self) {
        unsafe {
            self.dealloc_buffer();
        }
    }
}

// We need to guarantee the following:
// * We don't ever allocate `> isize::MAX` byte-size objects.
// * We don't overflow `usize::MAX` and actually allocate too little.
//
// On 64-bit we just need to check for overflow since trying to allocate
// `> isize::MAX` bytes will surely fail. On 32-bit and 16-bit we need to add
// an extra guard for this in case we're running on a platform which can use
// all 4GB in user-space, e.g., PAE or x32.

#[inline]
#[allow(clippy::cast_sign_loss)]
fn alloc_guard(alloc_size: usize) -> Result<(), CapacityOverflow> {
    if mem::size_of::<usize>() < 8 && alloc_size > isize::max_value() as usize {
        Err(CapacityOverflow)
    } else {
        Ok(())
    }
}

// One central function responsible for reporting capacity overflows. This'll
// ensure that the code generation related to these panics is minimal as there's
// only one location which panics rather than a bunch throughout the module.
fn capacity_overflow() -> ! {
    panic!("capacity overflow");
}
