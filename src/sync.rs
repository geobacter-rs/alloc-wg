//#![stable(feature = "rust1", since = "1.0.0")]

//! Thread-safe reference-counting pointers.
//!
//! See the [`Arc<T>`][Arc] documentation for more details.

use core::alloc::{AllocError, AllocRef, Layout};
use core::any::Any;
use core::borrow;
use core::cmp::Ordering;
use core::convert::{From, TryFrom};
use core::fmt;
use core::hash::{Hash, Hasher};
use core::intrinsics::abort;
use core::iter;
use core::marker::{PhantomData, Unpin, Unsize};
use core::mem::{self, align_of, align_of_val, MaybeUninit, size_of_val};
use core::ops::{CoerceUnsized, Deref, DispatchFromDyn, Receiver};
use core::pin::Pin;
use core::ptr::{self, NonNull, Unique};
use core::slice::from_raw_parts_mut;
use core::sync::atomic;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};

use crate::alloc::{handle_alloc_error, Global};
use crate::borrow::{Cow, ToOwned};
use crate::boxed::Box;
use crate::collections::TryReserveError;
use crate::iter::FromIteratorIn;
use crate::string::String;
use crate::vec::{Vec, SpecExtend};
use crate::slice::from_raw_parts;

#[cfg(test)]
mod tests;

/// A soft limit on the amount of references that may be made to an `Arc`.
///
/// Going above this limit will abort your program (although not
/// necessarily) at _exactly_ `MAX_REFCOUNT + 1` references.
const MAX_REFCOUNT: usize = (isize::MAX) as usize;

#[cfg(not(sanitize = "thread"))]
macro_rules! acquire {
    ($x:expr) => {
        atomic::fence(Acquire)
    };
}

// ThreadSanitizer does not support memory fences. To avoid false positive
// reports in Arc / Weak implementation use atomic loads for synchronization
// instead.
#[cfg(sanitize = "thread")]
macro_rules! acquire {
    ($x:expr) => {
        $x.load(Acquire)
    };
}

/// A thread-safe reference-counting pointer. 'Arc' stands for 'Atomically
/// Reference Counted'.
///
/// The type `Arc<T>` provides shared ownership of a value of type `T`,
/// allocated in the heap. Invoking [`clone`][clone] on `Arc` produces
/// a new `Arc` instance, which points to the same allocation on the heap as the
/// source `Arc`, while increasing a reference count. When the last `Arc`
/// pointer to a given allocation is destroyed, the value stored in that allocation (often
/// referred to as "inner value") is also dropped.
///
/// Shared references in Rust disallow mutation by default, and `Arc` is no
/// exception: you cannot generally obtain a mutable reference to something
/// inside an `Arc`. If you need to mutate through an `Arc`, use
/// [`Mutex`][mutex], [`RwLock`][rwlock], or one of the [`Atomic`][atomic]
/// types.
///
/// ## Thread Safety
///
/// Unlike [`Rc<T>`], `Arc<T>` uses atomic operations for its reference
/// counting. This means that it is thread-safe. The disadvantage is that
/// atomic operations are more expensive than ordinary memory accesses. If you
/// are not sharing reference-counted allocations between threads, consider using
/// [`Rc<T>`] for lower overhead. [`Rc<T>`] is a safe default, because the
/// compiler will catch any attempt to send an [`Rc<T>`] between threads.
/// However, a library might choose `Arc<T>` in order to give library consumers
/// more flexibility.
///
/// `Arc<T>` will implement [`Send`] and [`Sync`] as long as the `T` implements
/// [`Send`] and [`Sync`]. Why can't you put a non-thread-safe type `T` in an
/// `Arc<T>` to make it thread-safe? This may be a bit counter-intuitive at
/// first: after all, isn't the point of `Arc<T>` thread safety? The key is
/// this: `Arc<T>` makes it thread safe to have multiple ownership of the same
/// data, but it  doesn't add thread safety to its data. Consider
/// `Arc<`[`RefCell<T>`]`>`. [`RefCell<T>`] isn't [`Sync`], and if `Arc<T>` was always
/// [`Send`], `Arc<`[`RefCell<T>`]`>` would be as well. But then we'd have a problem:
/// [`RefCell<T>`] is not thread safe; it keeps track of the borrowing count using
/// non-atomic operations.
///
/// In the end, this means that you may need to pair `Arc<T>` with some sort of
/// [`std::sync`] type, usually [`Mutex<T>`][mutex].
///
/// ## Breaking cycles with `Weak`
///
/// The [`downgrade`][downgrade] method can be used to create a non-owning
/// [`Weak`] pointer. A [`Weak`] pointer can be [`upgrade`][upgrade]d
/// to an `Arc`, but this will return [`None`] if the value stored in the allocation has
/// already been dropped. In other words, `Weak` pointers do not keep the value
/// inside the allocation alive; however, they *do* keep the allocation
/// (the backing store for the value) alive.
///
/// A cycle between `Arc` pointers will never be deallocated. For this reason,
/// [`Weak`] is used to break cycles. For example, a tree could have
/// strong `Arc` pointers from parent nodes to children, and [`Weak`]
/// pointers from children back to their parents.
///
/// # Cloning references
///
/// Creating a new reference from an existing reference-counted pointer is done using the
/// `Clone` trait implemented for [`Arc<T>`][Arc] and [`Weak<T>`][Weak].
///
/// ```
/// use alloc_wg::sync::Arc;
/// let foo = Arc::new(vec![1.0, 2.0, 3.0]);
/// // The two syntaxes below are equivalent.
/// let a = foo.clone();
/// let b = Arc::clone(&foo);
/// // a, b, and foo are all Arcs that point to the same memory location
/// ```
///
/// ## `Deref` behavior
///
/// `Arc<T>` automatically dereferences to `T` (via the [`Deref`][deref] trait),
/// so you can call `T`'s methods on a value of type `Arc<T>`. To avoid name
/// clashes with `T`'s methods, the methods of `Arc<T>` itself are associated
/// functions, called using function-like syntax:
///
/// ```
/// use alloc_wg::sync::Arc;
/// let my_arc = Arc::new(());
///
/// Arc::downgrade(&my_arc);
/// ```
///
/// [`Weak<T>`][Weak] does not auto-dereference to `T`, because the inner value may have
/// already been dropped.
///
/// [`Rc<T>`]: crate::rc::Rc
/// [clone]: Clone::clone
/// [mutex]: ../../std/sync/struct.Mutex.html
/// [rwlock]: ../../std/sync/struct.RwLock.html
/// [atomic]: core::sync::atomic
/// [`Send`]: core::marker::Send
/// [`Sync`]: core::marker::Sync
/// [deref]: core::ops::Deref
/// [downgrade]: Arc::downgrade
/// [upgrade]: Weak::upgrade
/// [`RefCell<T>`]: core::cell::RefCell
/// [`std::sync`]: ../../std/sync/index.html
/// [`Arc::clone(&from)`]: Arc::clone
///
/// # Examples
///
/// Sharing some immutable data between threads:
///
// Note that we **do not** run these tests here. The windows builders get super
// unhappy if a thread outlives the main thread and then exits at the same time
// (something deadlocks) so we just avoid this entirely by not running these
// tests.
/// ```no_run
/// use alloc_wg::sync::Arc;
/// use std::thread;
///
/// let five = Arc::new(5);
///
/// for _ in 0..10 {
///     let five = Arc::clone(&five);
///
///     thread::spawn(move || {
///         println!("{:?}", five);
///     });
/// }
/// ```
///
/// Sharing a mutable [`AtomicUsize`]:
///
/// [`AtomicUsize`]: core::sync::atomic::AtomicUsize
///
/// ```no_run
/// use alloc_wg::sync::Arc;
/// use std::sync::atomic::{AtomicUsize, Ordering};
/// use std::thread;
///
/// let val = Arc::new(AtomicUsize::new(5));
///
/// for _ in 0..10 {
///     let val = Arc::clone(&val);
///
///     thread::spawn(move || {
///         let v = val.fetch_add(1, Ordering::SeqCst);
///         println!("{:?}", v);
///     });
/// }
/// ```
///
/// See the [`rc` documentation][rc_examples] for more examples of reference
/// counting in general.
///
/// [rc_examples]: crate::rc#examples
//#[cfg_attr(not(test), rustc_diagnostic_item = "Arc")]
//#[stable(feature = "rust1", since = "1.0.0")]
pub struct Arc<T: ?Sized, A: AllocRef = Global> {
    ptr: NonNull<ArcInner<T, A>>,
    phantom: PhantomData<ArcInner<T, A>>,
}

//#[stable(feature = "rust1", since = "1.0.0")]
unsafe impl<T: ?Sized + Sync + Send, A: AllocRef + Send> Send for Arc<T, A> {}
//#[stable(feature = "rust1", since = "1.0.0")]
unsafe impl<T: ?Sized + Sync + Send, A: AllocRef> Sync for Arc<T, A> {}

//#[unstable(feature = "coerce_unsized", issue = "27732")]
impl<T: ?Sized + Unsize<U>, U: ?Sized, A: AllocRef> CoerceUnsized<Arc<U, A>> for Arc<T, A> {}

//#[unstable(feature = "dispatch_from_dyn", issue = "none")]
impl<T: ?Sized + Unsize<U>, U: ?Sized, A: AllocRef> DispatchFromDyn<Arc<U, A>> for Arc<T, A> {}

impl<T: ?Sized, A: AllocRef> Arc<T, A> {
    fn from_inner(ptr: NonNull<ArcInner<T, A>>) -> Self {
        Self { ptr, phantom: PhantomData }
    }

    unsafe fn from_ptr(ptr: *mut ArcInner<T, A>) -> Self {
        Self::from_inner(NonNull::new_unchecked(ptr))
    }
}

/// `Weak` is a version of [`Arc`] that holds a non-owning reference to the
/// managed allocation. The allocation is accessed by calling [`upgrade`] on the `Weak`
/// pointer, which returns an [`Option`]`<`[`Arc`]`<T>>`.
///
/// Since a `Weak` reference does not count towards ownership, it will not
/// prevent the value stored in the allocation from being dropped, and `Weak` itself makes no
/// guarantees about the value still being present. Thus it may return [`None`]
/// when [`upgrade`]d. Note however that a `Weak` reference *does* prevent the allocation
/// itself (the backing store) from being deallocated.
///
/// A `Weak` pointer is useful for keeping a temporary reference to the allocation
/// managed by [`Arc`] without preventing its inner value from being dropped. It is also used to
/// prevent circular references between [`Arc`] pointers, since mutual owning references
/// would never allow either [`Arc`] to be dropped. For example, a tree could
/// have strong [`Arc`] pointers from parent nodes to children, and `Weak`
/// pointers from children back to their parents.
///
/// The typical way to obtain a `Weak` pointer is to call [`Arc::downgrade`].
///
/// [`upgrade`]: Weak::upgrade
//#[stable(feature = "arc_weak", since = "1.4.0")]
pub struct Weak<T: ?Sized, A: AllocRef = Global> {
    // This is a `NonNull` to allow optimizing the size of this type in enums,
    // but it is not necessarily a valid pointer.
    // `Weak::new` sets this to `usize::MAX` so that it doesn’t need
    // to allocate space on the heap.  That's not a value a real pointer
    // will ever have because RcBox has alignment at least 2.
    // This is only possible when `T: Sized`; unsized `T` never dangle.
    ptr: NonNull<ArcInner<T, A>>,
}

/// Note: A does *not* require Sync, ever. This is because it can only be used
/// in a single thread: the thread that drops the last weak reference. Thus it
/// is never truly shared.
//#[stable(feature = "arc_weak", since = "1.4.0")]
unsafe impl<T: ?Sized + Sync + Send, A: AllocRef + Send> Send for Weak<T, A> {}
//#[stable(feature = "arc_weak", since = "1.4.0")]
unsafe impl<T: ?Sized + Sync + Send, A: AllocRef> Sync for Weak<T, A> {}

//#[unstable(feature = "coerce_unsized", issue = "27732")]
impl<T: ?Sized + Unsize<U>, U: ?Sized, A: AllocRef> CoerceUnsized<Weak<U, A>> for Weak<T, A> {}
//#[unstable(feature = "dispatch_from_dyn", issue = "none")]
impl<T: ?Sized + Unsize<U>, U: ?Sized, A: AllocRef> DispatchFromDyn<Weak<U, A>> for Weak<T, A> {}

//#[stable(feature = "arc_weak", since = "1.4.0")]
impl<T: ?Sized + fmt::Debug, A: AllocRef> fmt::Debug for Weak<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(Weak)")
    }
}

// This is repr(C) to future-proof against possible field-reordering, which
// would interfere with otherwise safe [into|from]_raw() of transmutable
// inner types.
#[repr(C)]
struct ArcInner<T: ?Sized, A: AllocRef> {
    alloc: A,

    strong: atomic::AtomicUsize,

    // the value usize::MAX acts as a sentinel for temporarily "locking" the
    // ability to upgrade weak pointers or downgrade strong ones; this is used
    // to avoid races in `make_mut` and `get_mut`.
    weak: atomic::AtomicUsize,

    data: T,
}

impl<T, A: AllocRef> ArcInner<T, A> {
    fn new_in(strong: usize, weak: usize, data: T, alloc: A) -> NonNull<Self> {
        let (inner_ptr, alloc): (NonNull<MaybeUninit<ArcInner<T, A>>>, _) =
          Box::into_raw_non_null_alloc(Box::new_uninit_in(alloc));
        unsafe {
            let inner_ptr = inner_ptr.as_ptr() as *mut ArcInner<T, A>;
            inner_ptr.write(ArcInner {
                strong: atomic::AtomicUsize::new(strong),
                weak: atomic::AtomicUsize::new(weak),
                alloc,
                data,
            });
        }
        inner_ptr.cast()
    }
    fn try_new_in(strong: usize, weak: usize, data: T, alloc: A)
        -> Result<NonNull<Self>, TryReserveError>
    {
        let err = TryReserveError::AllocError { layout: Layout::new::<MaybeUninit<T>>(), };
        let b = Box::try_new_uninit_in(alloc)
          .map_err(move |_| err )?;
        let (inner_ptr, alloc): (NonNull<MaybeUninit<ArcInner<T, A>>>, _) =
          Box::into_raw_non_null_alloc(b);
        unsafe {
            let inner_ptr = inner_ptr.as_ptr() as *mut ArcInner<T, A>;
            inner_ptr.write(ArcInner {
                strong: atomic::AtomicUsize::new(strong),
                weak: atomic::AtomicUsize::new(weak),
                alloc,
                data,
            });
        }
        Ok(inner_ptr.cast())
    }
}

unsafe impl<T: ?Sized + Sync + Send, A: AllocRef + Send> Send for ArcInner<T, A> {}
unsafe impl<T: ?Sized + Sync + Send, A: AllocRef> Sync for ArcInner<T, A> {}

impl<T> Arc<T> {
    /// Constructs a new `Arc<T>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    /// ```
    #[inline]
    //#[stable(feature = "rust1", since = "1.0.0")]
    pub fn new(data: T) -> Arc<T> {
        Self::new_in(data, Global)
    }

    /// Constructs a new `Arc<T>` using a weak reference to itself. Attempting
    /// to upgrade the weak reference before this function returns will result
    /// in a `None` value. However, the weak reference may be cloned freely and
    /// stored for use at a later time.
    ///
    /// # Examples
    /// ```
    /// #![feature(arc_new_cyclic)]
    /// #![allow(dead_code)]
    ///
    /// use alloc_wg::sync::{Arc, Weak};
    ///
    /// struct Foo {
    ///     me: Weak<Foo>,
    /// }
    ///
    /// let foo = Arc::new_cyclic(|me| Foo {
    ///     me: me.clone(),
    /// });
    /// ```
    #[inline(always)]
    //#[unstable(feature = "arc_new_cyclic", issue = "75861")]
    pub fn new_cyclic(data_fn: impl FnOnce(&Weak<T>) -> T) -> Arc<T> {
        Self::new_cyclic_in(data_fn, Global)
    }

    /// Constructs a new `Arc` with uninitialized contents.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    /// #![feature(get_mut_unchecked)]
    ///
    /// use alloc_wg::sync::Arc;
    ///
    /// let mut five = Arc::<u32>::new_uninit();
    ///
    /// let five = unsafe {
    ///     // Deferred initialization:
    ///     Arc::get_mut_unchecked(&mut five).as_mut_ptr().write(5);
    ///
    ///     five.assume_init()
    /// };
    ///
    /// assert_eq!(*five, 5)
    /// ```
    //#[unstable(feature = "new_uninit", issue = "63291")]
    pub fn new_uninit() -> Arc<MaybeUninit<T>> {
        Self::new_uninit_in(Global)
    }

    /// Constructs a new `Arc` with uninitialized contents, with the memory
    /// being filled with `0` bytes.
    ///
    /// See [`MaybeUninit::zeroed`][zeroed] for examples of correct and incorrect usage
    /// of this method.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    ///
    /// use alloc_wg::sync::Arc;
    ///
    /// let zero = Arc::<u32>::new_zeroed();
    /// let zero = unsafe { zero.assume_init() };
    ///
    /// assert_eq!(*zero, 0)
    /// ```
    ///
    /// [zeroed]: ../../std/mem/union.MaybeUninit.html#method.zeroed
    //#[unstable(feature = "new_uninit", issue = "63291")]
    pub fn new_zeroed() -> Arc<MaybeUninit<T>> {
        Self::new_zeroed_in(Global)
    }

    /// Constructs a new `Pin<Arc<T>>`. If `T` does not implement `Unpin`, then
    /// `data` will be pinned in memory and unable to be moved.
    //#[stable(feature = "pin", since = "1.33.0")]
    pub fn pin(data: T) -> Pin<Arc<T>> {
        unsafe { Pin::new_unchecked(Arc::new(data)) }
    }
}
impl<T, A: AllocRef> Arc<T, A> {
    /// Constructs a new `Arc<T>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    /// ```
    #[inline]
    pub fn new_in(data: T, alloc: A) -> Arc<T, A> {
        // Start the weak pointer count as 1 which is the weak pointer that's
        // held by all the strong pointers (kinda), see std/rc.rs for more info
        Self::from_inner(ArcInner::new_in(1, 1, data, alloc))
    }
    #[inline]
    pub fn try_new_in(data: T, alloc: A) -> Result<Arc<T, A>, TryReserveError> {
        // Start the weak pointer count as 1 which is the weak pointer that's
        // held by all the strong pointers (kinda), see std/rc.rs for more info
        Ok(Self::from_inner(ArcInner::try_new_in(1, 1, data, alloc)?))
    }

    #[inline]
    fn init_cyclic_in(init_ptr: NonNull<ArcInner<T, A>>,
                      data_fn: impl FnOnce(&Weak<T, A>) -> T) -> Arc<T, A>
    {
        let weak = Weak { ptr: init_ptr };

        // It's important we don't give up ownership of the weak pointer, or
        // else the memory might be freed by the time `data_fn` returns. If
        // we really wanted to pass ownership, we could create an additional
        // weak pointer for ourselves, but this would result in additional
        // updates to the weak reference count which might not be necessary
        // otherwise.
        let data = data_fn(&weak);

        // Now we can properly initialize the inner value and turn our weak
        // reference into a strong reference.
        unsafe {
            let inner = init_ptr.as_ptr();
            ptr::write(&raw mut (*inner).data, data);

            // The above write to the data field must be visible to any threads which
            // observe a non-zero strong count. Therefore we need at least "Release" ordering
            // in order to synchronize with the `compare_exchange_weak` in `Weak::upgrade`.
            //
            // "Acquire" ordering is not required. When considering the possible behaviours
            // of `data_fn` we only need to look at what it could do with a reference to a
            // non-upgradeable `Weak`:
            // - It can *clone* the `Weak`, increasing the weak reference count.
            // - It can drop those clones, decreasing the weak reference count (but never to zero).
            //
            // These side effects do not impact us in any way, and no other side effects are
            // possible with safe code alone.
            let prev_value = (*inner).strong.fetch_add(1, Release);
            debug_assert_eq!(prev_value, 0, "No prior strong references should exist");
        }

        let strong = Arc::from_inner(init_ptr);

        // Strong references should collectively own a shared weak reference,
        // so don't run the destructor for our old weak reference.
        mem::forget(weak);
        strong
    }

    /// Constructs a new `Arc<T>` using a weak reference to itself. Attempting
    /// to upgrade the weak reference before this function returns will result
    /// in a `None` value. However, the weak reference may be cloned freely and
    /// stored for use at a later time.
    ///
    /// # Examples
    /// ```
    /// #![feature(arc_new_cyclic)]
    /// #![allow(dead_code)]
    ///
    /// use std::sync::{Arc, Weak};
    ///
    /// struct Foo {
    ///     me: Weak<Foo>,
    /// }
    ///
    /// let foo = Arc::new_cyclic(|me| Foo {
    ///     me: me.clone(),
    /// });
    /// ```
    #[inline]
    //#[unstable(feature = "arc_new_cyclic", issue = "75861")]
    pub fn new_cyclic_in(data_fn: impl FnOnce(&Weak<T, A>) -> T, alloc: A) -> Arc<T, A> {
        // Construct the inner in the "uninitialized" state with a single
        // weak reference.
        let uninit_ptr = ArcInner::new_in(0, 1, MaybeUninit::<T>::uninit(), alloc);
        Self::init_cyclic_in(uninit_ptr.cast(), data_fn)
    }
    #[inline]
    pub fn try_new_cyclic_in(data_fn: impl FnOnce(&Weak<T, A>) -> T, alloc: A)
        -> Result<Arc<T, A>, TryReserveError>
    {
        // Construct the inner in the "uninitialized" state with a single
        // weak reference.
        let uninit_ptr = ArcInner::try_new_in(0, 1, MaybeUninit::<T>::uninit(), alloc)?;
        Ok(Self::init_cyclic_in(uninit_ptr.cast(), data_fn))
    }

    /// Constructs a new `Arc` with uninitialized contents.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    /// #![feature(get_mut_unchecked)]
    ///
    /// use alloc_wg::sync::Arc;
    ///
    /// let mut five = Arc::<u32>::new_uninit();
    ///
    /// let five = unsafe {
    ///     // Deferred initialization:
    ///     Arc::get_mut_unchecked(&mut five).as_mut_ptr().write(5);
    ///
    ///     five.assume_init()
    /// };
    ///
    /// assert_eq!(*five, 5)
    /// ```
    pub fn new_uninit_in(alloc: A) -> Arc<MaybeUninit<T>, A> {
        unsafe {
            Arc::from_ptr(Arc::allocate_for_layout(
                Layout::new::<T>(),
                alloc,
                A::alloc,
                |mem| mem as *mut ArcInner<MaybeUninit<T>, A>,
            ))
        }
    }
    pub fn try_new_uninit_in(alloc: A) -> Result<Arc<MaybeUninit<T>, A>, TryReserveError> {
        unsafe {
            let ptr = Arc::try_allocate_for_layout(
                Layout::new::<T>(),
                alloc,
                A::alloc,
                |mem| mem as *mut ArcInner<MaybeUninit<T>, A>,
            )
              .map_err(map_error);
            Ok(Arc::from_ptr(ptr?))
        }
    }

    /// Constructs a new `Arc` with uninitialized contents, with the memory
    /// being filled with `0` bytes.
    ///
    /// See [`MaybeUninit::zeroed`][zeroed] for examples of correct and incorrect usage
    /// of this method.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    ///
    /// use alloc_wg::sync::Arc;
    ///
    /// let zero = Arc::<u32>::new_zeroed();
    /// let zero = unsafe { zero.assume_init() };
    ///
    /// assert_eq!(*zero, 0)
    /// ```
    ///
    /// [zeroed]: ../../std/mem/union.MaybeUninit.html#method.zeroed
    //#[unstable(feature = "new_uninit", issue = "63291")]
    pub fn new_zeroed_in(alloc: A) -> Arc<MaybeUninit<T>, A> {
        unsafe {
            Arc::from_ptr(Arc::allocate_for_layout(
                Layout::new::<T>(),
                alloc,
                A::alloc_zeroed,
                |mem| mem as *mut ArcInner<MaybeUninit<T>, A>,
            ))
        }
    }
    pub fn try_new_zero_in(alloc: A) -> Result<Arc<MaybeUninit<T>, A>, TryReserveError> {
        unsafe {
            let ptr = Arc::try_allocate_for_layout(
                Layout::new::<T>(),
                alloc,
                A::alloc_zeroed,
                |mem| mem as *mut ArcInner<MaybeUninit<T>, A>,
            )
              .map_err(map_error);
            Ok(Arc::from_ptr(ptr?))
        }
    }

    /// Constructs a new `Pin<Arc<T>>`. If `T` does not implement `Unpin`, then
    /// `data` will be pinned in memory and unable to be moved.
    //#[stable(feature = "pin", since = "1.33.0")]
    pub fn pin_in(data: T, alloc: A) -> Pin<Arc<T, A>> {
        unsafe { Pin::new_unchecked(Arc::new_in(data, alloc)) }
    }
    pub fn try_pin_in(data: T, alloc: A) -> Result<Pin<Arc<T, A>>, TryReserveError> {
        Ok(unsafe { Pin::new_unchecked(Arc::try_new_in(data, alloc)?) })
    }

    /// Returns the inner value, if the `Arc` has exactly one strong reference.
    ///
    /// Otherwise, an [`Err`] is returned with the same `Arc` that was
    /// passed in.
    ///
    /// This will succeed even if there are outstanding weak references.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let x = Arc::new(3);
    /// assert_eq!(Arc::try_unwrap(x), Ok(3));
    ///
    /// let x = Arc::new(4);
    /// let _y = Arc::clone(&x);
    /// assert_eq!(*Arc::try_unwrap(x).unwrap_err(), 4);
    /// ```
    #[inline]
    //#[stable(feature = "arc_unique", since = "1.4.0")]
    pub fn try_unwrap(this: Self) -> Result<T, Self> {
        if this.inner().strong.compare_exchange(1, 0, Relaxed, Relaxed).is_err() {
            return Err(this);
        }

        acquire!(this.inner().strong);

        unsafe {
            let elem = ptr::read(&this.ptr.as_ref().data);
            let _alloc = ptr::read(&this.ptr.as_ref().alloc);

            // Make a weak pointer to clean up the implicit strong-weak reference
            let _weak = Weak { ptr: this.ptr };
            mem::forget(this);

            Ok(elem)
        }
    }
    /// Returns the inner value, if the `Arc` has exactly one strong reference.
    ///
    /// Otherwise, an [`Err`] is returned with the same `Arc` that was
    /// passed in.
    ///
    /// This will succeed even if there are outstanding weak references.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let x = Arc::new(3);
    /// assert_eq!(Arc::try_unwrap(x), Ok(3));
    ///
    /// let x = Arc::new(4);
    /// let _y = Arc::clone(&x);
    /// assert_eq!(*Arc::try_unwrap(x).unwrap_err(), 4);
    /// ```
    #[inline]
    //#[stable(feature = "arc_unique", since = "1.4.0")]
    pub fn try_unwrap_alloc(this: Self) -> Result<(T, A), Self> {
        if this.inner().strong.compare_exchange(1, 0, Relaxed, Relaxed).is_err() {
            return Err(this);
        }

        acquire!(this.inner().strong);

        unsafe {
            let elem = ptr::read(&this.ptr.as_ref().data);
            let alloc = ptr::read(&this.ptr.as_ref().alloc);

            // Make a weak pointer to clean up the implicit strong-weak reference
            let _weak = Weak { ptr: this.ptr };
            mem::forget(this);

            Ok((elem, alloc))
        }
    }
}
impl<T> Arc<[T]> {
    /// Constructs a new atomically reference-counted slice with uninitialized contents.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    /// #![feature(get_mut_unchecked)]
    ///
    /// use std::sync::Arc;
    ///
    /// let mut values = Arc::<[u32]>::new_uninit_slice(3);
    ///
    /// let values = unsafe {
    ///     // Deferred initialization:
    ///     Arc::get_mut_unchecked(&mut values)[0].as_mut_ptr().write(1);
    ///     Arc::get_mut_unchecked(&mut values)[1].as_mut_ptr().write(2);
    ///     Arc::get_mut_unchecked(&mut values)[2].as_mut_ptr().write(3);
    ///
    ///     values.assume_init()
    /// };
    ///
    /// assert_eq!(*values, [1, 2, 3])
    /// ```
    //#[unstable(feature = "new_uninit", issue = "63291")]
    #[inline(always)]
    pub fn new_uninit_slice(len: usize) -> Arc<[MaybeUninit<T>]> {
        Self::new_uninit_slice_in(len, Global)
    }
    #[inline(always)]
    pub fn try_new_uninit_slice(len: usize) -> Result<Arc<[MaybeUninit<T>]>, TryReserveError> {
        Self::try_new_uninit_slice_in(len, Global)
    }

    /// Constructs a new atomically reference-counted slice with uninitialized contents, with the memory being
    /// filled with `0` bytes.
    ///
    /// See [`MaybeUninit::zeroed`][zeroed] for examples of correct and
    /// incorrect usage of this method.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    ///
    /// use std::sync::Arc;
    ///
    /// let values = Arc::<[u32]>::new_zeroed_slice(3);
    /// let values = unsafe { values.assume_init() };
    ///
    /// assert_eq!(*values, [0, 0, 0])
    /// ```
    ///
    /// [zeroed]: ../../std/mem/union.MaybeUninit.html#method.zeroed
    //#[unstable(feature = "new_uninit", issue = "63291")]
    #[inline(always)]
    pub fn new_zeroed_slice(len: usize) -> Arc<[MaybeUninit<T>]> {
        Self::new_zeroed_slice_in(len, Global)
    }
}
impl<T, A: AllocRef> Arc<[T], A> {
    /// Constructs a new atomically reference-counted slice with uninitialized contents.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    /// #![feature(get_mut_unchecked)]
    ///
    /// use std::sync::Arc;
    ///
    /// let mut values = Arc::<[u32]>::new_uninit_slice(3);
    ///
    /// let values = unsafe {
    ///     // Deferred initialization:
    ///     Arc::get_mut_unchecked(&mut values)[0].as_mut_ptr().write(1);
    ///     Arc::get_mut_unchecked(&mut values)[1].as_mut_ptr().write(2);
    ///     Arc::get_mut_unchecked(&mut values)[2].as_mut_ptr().write(3);
    ///
    ///     values.assume_init()
    /// };
    ///
    /// assert_eq!(*values, [1, 2, 3])
    /// ```
    //#[unstable(feature = "new_uninit", issue = "63291")]
    pub fn new_uninit_slice_in(len: usize, alloc: A) -> Arc<[MaybeUninit<T>], A> {
        unsafe { Arc::from_ptr(Arc::allocate_for_slice(len, alloc)) }
    }
    pub fn try_new_uninit_slice_in(len: usize, alloc: A)
        -> Result<Arc<[MaybeUninit<T>], A>, TryReserveError>
    {

        Ok(unsafe {
            let ptr = Arc::try_allocate_for_slice(len, alloc)
              .map_err(map_error);
            Arc::from_ptr(ptr?)
        })
    }

    /// Constructs a new atomically reference-counted slice with uninitialized contents, with the memory being
    /// filled with `0` bytes.
    ///
    /// See [`MaybeUninit::zeroed`][zeroed] for examples of correct and
    /// incorrect usage of this method.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    ///
    /// use std::sync::Arc;
    ///
    /// let values = Arc::<[u32]>::new_zeroed_slice(3);
    /// let values = unsafe { values.assume_init() };
    ///
    /// assert_eq!(*values, [0, 0, 0])
    /// ```
    ///
    /// [zeroed]: ../../std/mem/union.MaybeUninit.html#method.zeroed
    //#[unstable(feature = "new_uninit", issue = "63291")]
    pub fn new_zeroed_slice_in(len: usize, alloc: A) -> Arc<[MaybeUninit<T>], A> {
        unsafe {
            Arc::from_ptr(Arc::allocate_for_layout(
                Layout::array::<T>(len).unwrap(),
                alloc,
                A::alloc_zeroed,
                |mem| {
                    ptr::slice_from_raw_parts_mut(mem as *mut T, len)
                        as *mut ArcInner<[MaybeUninit<T>], A>
                },
            ))
        }
    }
    pub fn try_new_zeroed_slice_in(len: usize, alloc: A)
        -> Result<Arc<[MaybeUninit<T>], A>, TryReserveError>
    {
        unsafe {
            let ptr = Arc::try_allocate_for_layout(
                Layout::array::<T>(len).unwrap(),
                alloc,
                A::alloc_zeroed,
                |mem| {
                    ptr::slice_from_raw_parts_mut(mem as *mut T, len)
                      as *mut ArcInner<[MaybeUninit<T>], A>
                },
            )
              .map_err(map_error);
            Ok(Arc::from_ptr(ptr?))
        }
    }
}

impl<T, A: AllocRef> Arc<MaybeUninit<T>, A> {
    /// Converts to `Arc<T>`.
    ///
    /// # Safety
    ///
    /// As with [`MaybeUninit::assume_init`],
    /// it is up to the caller to guarantee that the inner value
    /// really is in an initialized state.
    /// Calling this when the content is not yet fully initialized
    /// causes immediate undefined behavior.
    ///
    /// [`MaybeUninit::assume_init`]: ../../std/mem/union.MaybeUninit.html#method.assume_init
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    /// #![feature(get_mut_unchecked)]
    ///
    /// use std::sync::Arc;
    ///
    /// let mut five = Arc::<u32>::new_uninit();
    ///
    /// let five = unsafe {
    ///     // Deferred initialization:
    ///     Arc::get_mut_unchecked(&mut five).as_mut_ptr().write(5);
    ///
    ///     five.assume_init()
    /// };
    ///
    /// assert_eq!(*five, 5)
    /// ```
    //#[unstable(feature = "new_uninit", issue = "63291")]
    #[inline]
    pub unsafe fn assume_init(self) -> Arc<T, A> {
        Arc::from_inner(mem::ManuallyDrop::new(self).ptr.cast())
    }
}

impl<T, A: AllocRef> Arc<[MaybeUninit<T>], A> {
    /// Converts to `Arc<[T]>`.
    ///
    /// # Safety
    ///
    /// As with [`MaybeUninit::assume_init`],
    /// it is up to the caller to guarantee that the inner value
    /// really is in an initialized state.
    /// Calling this when the content is not yet fully initialized
    /// causes immediate undefined behavior.
    ///
    /// [`MaybeUninit::assume_init`]: ../../std/mem/union.MaybeUninit.html#method.assume_init
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(new_uninit)]
    /// #![feature(get_mut_unchecked)]
    ///
    /// use std::sync::Arc;
    ///
    /// let mut values = Arc::<[u32]>::new_uninit_slice(3);
    ///
    /// let values = unsafe {
    ///     // Deferred initialization:
    ///     Arc::get_mut_unchecked(&mut values)[0].as_mut_ptr().write(1);
    ///     Arc::get_mut_unchecked(&mut values)[1].as_mut_ptr().write(2);
    ///     Arc::get_mut_unchecked(&mut values)[2].as_mut_ptr().write(3);
    ///
    ///     values.assume_init()
    /// };
    ///
    /// assert_eq!(*values, [1, 2, 3])
    /// ```
    //#[unstable(feature = "new_uninit", issue = "63291")]
    #[inline]
    pub unsafe fn assume_init(self) -> Arc<[T], A> {
        Arc::from_ptr(mem::ManuallyDrop::new(self).ptr.as_ptr() as _)
    }
}

impl<T: ?Sized, A: AllocRef> Arc<T, A> {
    /// Consumes the `Arc`, returning the wrapped pointer.
    ///
    /// To avoid a memory leak the pointer must be converted back to an `Arc` using
    /// [`Arc::from_raw`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let x = Arc::new("hello".to_owned());
    /// let x_ptr = Arc::into_raw(x);
    /// assert_eq!(unsafe { &*x_ptr }, "hello");
    /// ```
    //#[stable(feature = "rc_raw", since = "1.17.0")]
    pub fn into_raw(this: Self) -> *const T {
        let ptr = Self::as_ptr(&this);
        mem::forget(this);
        ptr
    }

    /// Provides a raw pointer to the data.
    ///
    /// The counts are not affected in any way and the `Arc` is not consumed. The pointer is valid for
    /// as long as there are strong counts in the `Arc`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let x = Arc::new("hello".to_owned());
    /// let y = Arc::clone(&x);
    /// let x_ptr = Arc::as_ptr(&x);
    /// assert_eq!(x_ptr, Arc::as_ptr(&y));
    /// assert_eq!(unsafe { &*x_ptr }, "hello");
    /// ```
    //#[stable(feature = "rc_as_ptr", since = "1.45.0")]
    pub fn as_ptr(this: &Self) -> *const T {
        let ptr: *mut ArcInner<T, A> = NonNull::as_ptr(this.ptr);

        // SAFETY: This cannot go through Deref::deref or RcBoxPtr::inner because
        // this is required to retain raw/mut provenance such that e.g. `get_mut` can
        // write through the pointer after the Rc is recovered through `from_raw`.
        unsafe { &raw const (*ptr).data }
    }

    #[inline(always)]
    pub fn as_inner_ptr(this: &Self) -> NonNull<[u8]> {
        let ptr: NonNull<u8> = this.ptr.cast();
        let layout = unsafe { Layout::for_value_raw(this.ptr.as_ptr()) };
        NonNull::from(unsafe {
            from_raw_parts(ptr.as_ptr(), layout.size())
        })
    }

    /// Constructs an `Arc<T>` from a raw pointer.
    ///
    /// The raw pointer must have been previously returned by a call to
    /// [`Arc<U>::into_raw`][into_raw] where `U` must have the same size and
    /// alignment as `T`. This is trivially true if `U` is `T`.
    /// Note that if `U` is not `T` but has the same size and alignment, this is
    /// basically like transmuting references of different types. See
    /// [`mem::transmute`][transmute] for more information on what
    /// restrictions apply in this case.
    ///
    /// The user of `from_raw` has to make sure a specific value of `T` is only
    /// dropped once.
    ///
    /// This function is unsafe because improper use may lead to memory unsafety,
    /// even if the returned `Arc<T>` is never accessed.
    ///
    /// [into_raw]: Arc::into_raw
    /// [transmute]: core::mem::transmute
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let x = Arc::new("hello".to_owned());
    /// let x_ptr = Arc::into_raw(x);
    ///
    /// unsafe {
    ///     // Convert back to an `Arc` to prevent leak.
    ///     let x = Arc::from_raw(x_ptr);
    ///     assert_eq!(&*x, "hello");
    ///
    ///     // Further calls to `Arc::from_raw(x_ptr)` would be memory-unsafe.
    /// }
    ///
    /// // The memory was freed when `x` went out of scope above, so `x_ptr` is now dangling!
    /// ```
    //#[stable(feature = "rc_raw", since = "1.17.0")]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        let offset = data_offset::<_, A>(ptr);

        // Reverse the offset to find the original ArcInner.
        let fake_ptr = ptr as *mut ArcInner<T, A>;
        let arc_ptr = set_data_ptr(fake_ptr, (ptr as *mut u8).offset(-offset));

        Self::from_ptr(arc_ptr)
    }

    /// Creates a new [`Weak`] pointer to this allocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// let weak_five = Arc::downgrade(&five);
    /// ```
    //#[stable(feature = "arc_weak", since = "1.4.0")]
    pub fn downgrade(this: &Self) -> Weak<T, A> {
        // This Relaxed is OK because we're checking the value in the CAS
        // below.
        let mut cur = this.inner().weak.load(Relaxed);

        loop {
            // check if the weak counter is currently "locked"; if so, spin.
            if cur == usize::MAX {
                cur = this.inner().weak.load(Relaxed);
                continue;
            }

            // NOTE: this code currently ignores the possibility of overflow
            // into usize::MAX; in general both Rc and Arc need to be adjusted
            // to deal with overflow.

            // Unlike with Clone(), we need this to be an Acquire read to
            // synchronize with the write coming from `is_unique`, so that the
            // events prior to that write happen before this read.
            match this.inner().weak.compare_exchange_weak(cur, cur + 1, Acquire, Relaxed) {
                Ok(_) => {
                    // Make sure we do not create a dangling Weak
                    debug_assert!(!is_dangling(this.ptr));
                    return Weak { ptr: this.ptr };
                }
                Err(old) => cur = old,
            }
        }
    }

    /// Gets the number of [`Weak`] pointers to this allocation.
    ///
    /// # Safety
    ///
    /// This method by itself is safe, but using it correctly requires extra care.
    /// Another thread can change the weak count at any time,
    /// including potentially between calling this method and acting on the result.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5);
    /// let _weak_five = Arc::downgrade(&five);
    ///
    /// // This assertion is deterministic because we haven't shared
    /// // the `Arc` or `Weak` between threads.
    /// assert_eq!(1, Arc::weak_count(&five));
    /// ```
    #[inline]
    //#[stable(feature = "arc_counts", since = "1.15.0")]
    pub fn weak_count(this: &Self) -> usize {
        let cnt = this.inner().weak.load(SeqCst);
        // If the weak count is currently locked, the value of the
        // count was 0 just before taking the lock.
        if cnt == usize::MAX { 0 } else { cnt - 1 }
    }

    /// Gets the number of strong (`Arc`) pointers to this allocation.
    ///
    /// # Safety
    ///
    /// This method by itself is safe, but using it correctly requires extra care.
    /// Another thread can change the strong count at any time,
    /// including potentially between calling this method and acting on the result.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5);
    /// let _also_five = Arc::clone(&five);
    ///
    /// // This assertion is deterministic because we haven't shared
    /// // the `Arc` between threads.
    /// assert_eq!(2, Arc::strong_count(&five));
    /// ```
    #[inline]
    //#[stable(feature = "arc_counts", since = "1.15.0")]
    pub fn strong_count(this: &Self) -> usize {
        this.inner().strong.load(SeqCst)
    }

    /// Increments the strong reference count on the `Arc<T>` associated with the
    /// provided pointer by one.
    ///
    /// # Safety
    ///
    /// The pointer must have been obtained through `Arc::into_raw`, and the
    /// associated `Arc` instance must be valid (i.e. the strong count must be at
    /// least 1) for the duration of this method.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(arc_mutate_strong_count)]
    ///
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// unsafe {
    ///     let ptr = Arc::into_raw(five);
    ///     Arc::incr_strong_count(ptr);
    ///
    ///     // This assertion is deterministic because we haven't shared
    ///     // the `Arc` between threads.
    ///     let five = Arc::from_raw(ptr);
    ///     assert_eq!(2, Arc::strong_count(&five));
    /// }
    /// ```
    #[inline]
    //#[unstable(feature = "arc_mutate_strong_count", issue = "71983")]
    pub unsafe fn incr_strong_count(ptr: *const T) {
        // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
        let arc = mem::ManuallyDrop::new(Arc::<T>::from_raw(ptr));
        // Now increase refcount, but don't drop new refcount either
        let _arc_clone: mem::ManuallyDrop<_> = arc.clone();
    }

    /// Decrements the strong reference count on the `Arc<T>` associated with the
    /// provided pointer by one.
    ///
    /// # Safety
    ///
    /// The pointer must have been obtained through `Arc::into_raw`, and the
    /// associated `Arc` instance must be valid (i.e. the strong count must be at
    /// least 1) when invoking this method. This method can be used to release the final
    /// `Arc` and backing storage, but **should not** be called after the final `Arc` has been
    /// released.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(arc_mutate_strong_count)]
    /// #![feature(allocator_api)]
    ///
    /// use alloc_wg::sync::Arc;
    /// use std::alloc::Global;
    ///
    /// let five = Arc::new(5);
    ///
    /// unsafe {
    ///     let ptr = Arc::into_raw(five);
    ///     <Arc<_, Global>>::incr_strong_count(ptr);
    ///
    ///     // Those assertions are deterministic because we haven't shared
    ///     // the `Arc` between threads.
    ///     let five = <Arc<_, Global>>::from_raw(ptr);
    ///     assert_eq!(2, Arc::strong_count(&five));
    ///     <Arc<_, Global>>::decr_strong_count(ptr);
    ///     assert_eq!(1, Arc::strong_count(&five));
    /// }
    /// ```
    #[inline]
    //#[unstable(feature = "arc_mutate_strong_count", issue = "71983")]
    pub unsafe fn decr_strong_count(ptr: *const T) {
        drop(Self::from_raw(ptr));
    }

    #[inline]
    fn inner(&self) -> &ArcInner<T, A> {
        // This unsafety is ok because while this arc is alive we're guaranteed
        // that the inner pointer is valid. Furthermore, we know that the
        // `ArcInner` structure itself is `Sync` because the inner data is
        // `Sync` as well, so we're ok loaning out an immutable pointer to these
        // contents.
        unsafe { self.ptr.as_ref() }
    }
    #[inline]
    unsafe fn inner_mut(&mut self) -> &mut ArcInner<T, A> {
        self.ptr.as_mut()
    }

    // Non-inlined part of `drop`.
    #[inline(never)]
    unsafe fn drop_slow(&mut self) {
        // Destroy the data at this time, even though we may not free the box
        // allocation itself (there may still be weak pointers lying around).
        ptr::drop_in_place(Self::get_mut_unchecked(self));

        // Drop the weak ref collectively held by all strong references
        drop(Weak { ptr: self.ptr });
    }

    #[inline]
    //#[stable(feature = "ptr_eq", since = "1.17.0")]
    /// Returns `true` if the two `Arc`s point to the same allocation
    /// (in a vein similar to [`ptr::eq`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5);
    /// let same_five = Arc::clone(&five);
    /// let other_five = Arc::new(5);
    ///
    /// assert!(Arc::ptr_eq(&five, &same_five));
    /// assert!(!Arc::ptr_eq(&five, &other_five));
    /// ```
    ///
    /// [`ptr::eq`]: core::ptr::eq
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.ptr.as_ptr() == other.ptr.as_ptr()
    }

    #[inline(always)]
    pub fn alloc_ref(this: &Self) -> &A
        where A: Sync,
    {
        &this.inner().alloc
    }
}

impl<T: ?Sized, A: AllocRef> Arc<T, A> {
    /// Allocates an `ArcInner<T>` with sufficient space for
    /// a possibly-unsized inner value where the value has the layout provided.
    ///
    /// The function `mem_to_arcinner` is called with the data pointer
    /// and must return back a (potentially fat)-pointer for the `ArcInner<T>`.
    unsafe fn allocate_for_layout(
        value_layout: Layout,
        alloc: A,
        allocate: impl FnOnce(&A, Layout) -> Result<NonNull<[u8]>, AllocError>,
        mem_to_arcinner: impl FnOnce(*mut u8) -> *mut ArcInner<T, A>,
    ) -> *mut ArcInner<T, A> {
        Self::try_allocate_for_layout(value_layout, alloc, move |alloc, layout| {
            Ok(allocate(alloc, layout).unwrap_or_else(|_| handle_alloc_error(layout) ))
        }, mem_to_arcinner)
          .map_err(map_error)
          .unwrap()
    }
    /// Allocates an `ArcInner<T>` with sufficient space for
    /// a possibly-unsized inner value where the value has the layout provided.
    ///
    /// The function `mem_to_arcinner` is called with the data pointer
    /// and must return back a (potentially fat)-pointer for the `ArcInner<T>`.
    unsafe fn try_allocate_for_layout(
        value_layout: Layout,
        alloc: A,
        allocate: impl FnOnce(&A, Layout) -> Result<NonNull<[u8]>, AllocError>,
        mem_to_arcinner: impl FnOnce(*mut u8) -> *mut ArcInner<T, A>,
    ) -> Result<*mut ArcInner<T, A>, (TryReserveError, A)> {
        // Calculate layout using the given value layout.
        // Previously, layout was calculated on the expression
        // `&*(ptr as *const ArcInner<T>)`, but this created a misaligned
        // reference (see #54908).
        let layout = Layout::new::<ArcInner<(), A>>()
          .extend(value_layout)
          .unwrap().0
          .pad_to_align();

        let ptr = match allocate(&alloc, layout) {
            Ok(ptr) => ptr,
            Err(_) => {
                return Err((TryReserveError::AllocError {
                    layout,
                }, alloc));
            },
        };

        // Initialize the ArcInner
        let inner = mem_to_arcinner(ptr.as_non_null_ptr().as_ptr());
        debug_assert_eq!(Layout::for_value(&*inner), layout);

        ptr::write(&mut (*inner).alloc, alloc);
        ptr::write(&mut (*inner).strong, atomic::AtomicUsize::new(1));
        ptr::write(&mut (*inner).weak, atomic::AtomicUsize::new(1));


        Ok(inner)
    }

    /// Allocates an `ArcInner<T>` with sufficient space for an unsized inner value.
    unsafe fn allocate_for_ptr(ptr: *const T, alloc: A) -> *mut ArcInner<T, A> {
        // Allocate for the `ArcInner<T>` using the given value.
        Self::allocate_for_layout(
            Layout::for_value(&*ptr),
            alloc,
            A::alloc,
            |mem| set_data_ptr(ptr as *mut T, mem) as *mut ArcInner<T, A>,
        )
    }
    unsafe fn try_allocate_for_ptr(ptr: *const T, alloc: A)
        -> Result<*mut ArcInner<T, A>, (TryReserveError, A)>
    {
        // Allocate for the `ArcInner<T>` using the given value.
        Self::try_allocate_for_layout(
            Layout::for_value(&*ptr),
            alloc,
            A::alloc,
            |mem| set_data_ptr(ptr as *mut T, mem) as *mut ArcInner<T, A>,
        )
    }

    fn from_box(v: Box<T, A>) -> Arc<T, A> {
        unsafe {
            let (box_unique, alloc) = Box::into_unique_alloc(v);
            let bptr = box_unique.as_ptr();

            let value_size = size_of_val(&*bptr);
            let ptr = Self::allocate_for_ptr(bptr, alloc);

            // Copy value as bytes
            ptr::copy_nonoverlapping(
                bptr as *mut u8,
                &mut (*ptr).data as *mut _ as *mut u8,
                value_size,
            );

            // Free the allocation without dropping its contents
            box_free(box_unique);

            Self::from_ptr(ptr)
        }
    }
    fn try_from_box(v: Box<T, A>) -> Result<Arc<T, A>, TryReserveError> {
        unsafe {
            let (box_unique, alloc) = Box::into_unique_alloc(v);
            let bptr = box_unique.as_ptr();

            let value_size = size_of_val(&*bptr);
            let ptr = Self::try_allocate_for_ptr(bptr, alloc)
              .map_err(map_error)?;

            // Copy value as bytes
            ptr::copy_nonoverlapping(
                bptr as *mut u8,
                &mut (*ptr).data as *mut _ as *mut u8,
                value_size,
            );

            // Free the allocation without dropping its contents
            box_free(box_unique);

            Ok(Self::from_ptr(ptr))
        }
    }
}

impl<T, A: AllocRef> Arc<[T], A> {
    /// Allocates an `ArcInner<[T]>` with the given length.
    unsafe fn allocate_for_slice(len: usize, alloc: A) -> *mut ArcInner<[T], A> {
        Self::allocate_for_layout(
            Layout::array::<T>(len).unwrap(),
            alloc,
            A::alloc,
            |mem| ptr::slice_from_raw_parts_mut(mem as *mut T, len) as *mut ArcInner<[T], A>,
        )
    }
    unsafe fn try_allocate_for_slice(len: usize, alloc: A)
        -> Result<*mut ArcInner<[T], A>, (TryReserveError, A)>
    {
        Self::try_allocate_for_layout(
            Layout::array::<T>(len).unwrap(),
            alloc,
            A::alloc,
            |mem| ptr::slice_from_raw_parts_mut(mem as *mut T, len) as *mut ArcInner<[T], A>,
        )
    }
}
impl<A: AllocRef> Arc<str, A> {
    /// Allocates an `ArcInner<[T]>` with the given length.
    unsafe fn allocate_for_str(len: usize, alloc: A) -> *mut ArcInner<str, A> {
        Self::allocate_for_layout(
            Layout::array::<u8>(len).unwrap(),
            alloc,
            A::alloc,
            |mem| ptr::slice_from_raw_parts_mut(mem as *mut u8, len) as *mut ArcInner<str, A>,
        )
    }
    unsafe fn try_allocate_for_str(len: usize, alloc: A)
                                   -> Result<*mut ArcInner<str, A>, (TryReserveError, A)>
    {
        Self::try_allocate_for_layout(
            Layout::array::<u8>(len).unwrap(),
            alloc,
            A::alloc,
            |mem| ptr::slice_from_raw_parts_mut(mem as *mut u8, len) as *mut ArcInner<str, A>,
        )
    }
}

/// Sets the data pointer of a `?Sized` raw pointer.
///
/// For a slice/trait object, this sets the `data` field and leaves the rest
/// unchanged. For a sized raw pointer, this simply sets the pointer.
unsafe fn set_data_ptr<T: ?Sized, U>(mut ptr: *mut T, data: *mut U) -> *mut T {
    ptr::write(&mut ptr as *mut _ as *mut *mut u8, data as *mut u8);
    ptr
}

impl<T, A: AllocRef> Arc<[T], A> {
    /// Copy elements from slice into newly allocated Arc<\[T\]>
    ///
    /// Unsafe because the caller must either take ownership or bind `T: Copy`.
    unsafe fn copy_from_slice(v: &[T], alloc: A) -> Arc<[T], A> {
        let ptr = Self::allocate_for_slice(v.len(), alloc);

        ptr::copy_nonoverlapping(v.as_ptr(), &mut (*ptr).data as *mut [T] as *mut T, v.len());

        Self::from_ptr(ptr)
    }
    /// Copy elements from slice into newly allocated Arc<\[T\]>
    ///
    /// Unsafe because the caller must either take ownership or bind `T: Copy`.
    unsafe fn try_copy_from_slice(v: &[T], alloc: A) -> Result<Arc<[T], A>, (TryReserveError, A)> {
        let ptr = Self::try_allocate_for_slice(v.len(), alloc)?;

        ptr::copy_nonoverlapping(v.as_ptr(), &mut (*ptr).data as *mut [T] as *mut T, v.len());

        Ok(Self::from_ptr(ptr))
    }

    /// Constructs an `Arc<[T]>` from an iterator known to be of a certain size.
    ///
    /// Behavior is undefined should the size be wrong.
    unsafe fn try_from_iter_exact(iter: impl Iterator<Item = T>,
                                  len: usize, alloc: A) -> Result<Arc<[T], A>, TryReserveError> {
        // Panic guard while cloning T elements.
        // In the event of a panic, elements that have been written
        // into the new ArcInner will be dropped, then the memory freed.
        // There is a bit of trickery here to ensure `alloc` isn't leaked.
        struct Guard<T, A: AllocRef> {
            mem: NonNull<ArcInner<(), A>>,
            elems: *mut T,
            layout: Layout,
            n_elems: usize,
        }

        impl<T, A: AllocRef> Drop for Guard<T, A> {
            fn drop(&mut self) {
                unsafe {
                    let slice = from_raw_parts_mut(self.elems, self.n_elems);
                    ptr::drop_in_place(slice);

                    let mem = self.mem;

                    // don't dealloc before we've extracted the allocator instance.
                    let alloc = ptr::read(&mem.as_ref().alloc);

                    alloc.dealloc(mem.cast(), self.layout);
                }
            }
        }

        let ptr = Self::try_allocate_for_slice(len, alloc)
          .map_err(map_error)?;

        let layout = Layout::for_value(&*ptr);

        // Pointer to first element
        let elems = &mut (*ptr).data as *mut [T] as *mut T;

        let mut guard = Guard {
            mem: NonNull::new_unchecked(ptr).cast::<ArcInner<_, A>>(),
            elems,
            layout,
            n_elems: 0,
        };

        for (i, item) in iter.enumerate() {
            ptr::write(elems.add(i), item);
            guard.n_elems += 1;
        }

        // All clear. Forget the guard so it doesn't free the new ArcInner.
        mem::forget(guard);

        Ok(Self::from_ptr(ptr))
    }
    /// Constructs an `Arc<[T]>` from an iterator known to be of a certain size.
    ///
    /// Behavior is undefined should the size be wrong.
    unsafe fn from_iter_exact(iter: impl Iterator<Item = T>,
                              len: usize, alloc: A) -> Arc<[T], A> {
        // Panic guard while cloning T elements.
        // In the event of a panic, elements that have been written
        // into the new ArcInner will be dropped, then the memory freed.
        // There is a bit of trickery here to ensure `alloc` isn't leaked.
        struct Guard<T, A: AllocRef> {
            mem: NonNull<ArcInner<(), A>>,
            elems: *mut T,
            layout: Layout,
            n_elems: usize,
        }

        impl<T, A: AllocRef> Drop for Guard<T, A> {
            fn drop(&mut self) {
                unsafe {
                    let slice = from_raw_parts_mut(self.elems, self.n_elems);
                    ptr::drop_in_place(slice);

                    let mem = self.mem.cast::<ArcInner<(), A>>();

                    // don't dealloc before we've extracted the allocator instance.
                    let alloc = ptr::read(&mem.as_ref().alloc);

                    alloc.dealloc(mem.cast(), self.layout);
                }
            }
        }

        let ptr = Self::allocate_for_slice(len, alloc);

        let layout = Layout::for_value(&*ptr);

        // Pointer to first element
        let elems = &mut (*ptr).data as *mut [T] as *mut T;

        let mut guard = Guard {
            mem: NonNull::new_unchecked(ptr).cast::<ArcInner<_, A>>(),
            elems,
            layout,
            n_elems: 0,
        };

        for (i, item) in iter.enumerate() {
            ptr::write(elems.add(i), item);
            guard.n_elems += 1;
        }

        // All clear. Forget the guard so it doesn't free the new ArcInner.
        mem::forget(guard);

        Self::from_ptr(ptr)
    }
}
impl<A: AllocRef> Arc<str, A> {
    /// Copy elements from slice into newly allocated Arc<\[T\]>
    ///
    /// Unsafe because the caller must either take ownership or bind `T: Copy`.
    unsafe fn copy_from_str(v: &str, alloc: A) -> Arc<str, A> {
        let ptr = Self::allocate_for_str(v.len(), alloc);

        ptr::copy_nonoverlapping(v.as_ptr(), &mut (*ptr).data as *mut str as *mut u8, v.len());

        Self::from_ptr(ptr)
    }
    /// Copy elements from slice into newly allocated Arc<\[T\]>
    ///
    /// Unsafe because the caller must either take ownership or bind `T: Copy`.
    unsafe fn try_copy_from_str(v: &str, alloc: A) -> Result<Arc<str, A>, (TryReserveError, A)> {
        let ptr = Self::try_allocate_for_str(v.len(), alloc)?;

        ptr::copy_nonoverlapping(v.as_ptr(), &mut (*ptr).data as *mut str as *mut u8, v.len());

        Ok(Self::from_ptr(ptr))
    }
}

/// Specialization trait used for `From<&[T]>`.
trait ArcFromSlice<T> {
    fn from_slice(slice: &[T]) -> Self;
}

impl<T: Clone> ArcFromSlice<T> for Arc<[T]> {
    #[inline]
    default fn from_slice(v: &[T]) -> Self {
        unsafe {
            Self::from_iter_exact(v.iter().cloned(), v.len(),
                                  Default::default())
        }
    }
}

impl<T: Copy> ArcFromSlice<T> for Arc<[T]> {
    #[inline]
    fn from_slice(v: &[T]) -> Self {
        unsafe { Arc::copy_from_slice(v, Global) }
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized, A: AllocRef> Clone for Arc<T, A> {
    /// Makes a clone of the `Arc` pointer.
    ///
    /// This creates another pointer to the same allocation, increasing the
    /// strong reference count.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// let _ = Arc::clone(&five);
    /// ```
    #[inline]
    fn clone(&self) -> Arc<T, A> {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let old_size = self.inner().strong.fetch_add(1, Relaxed);

        // However we need to guard against massive refcounts in case someone
        // is `mem::forget`ing Arcs. If we don't do this the count can overflow
        // and users will use-after free. We racily saturate to `isize::MAX` on
        // the assumption that there aren't ~2 billion threads incrementing
        // the reference count at once. This branch will never be taken in
        // any realistic program.
        //
        // We abort because such a program is incredibly degenerate, and we
        // don't care to support it.
        if old_size > MAX_REFCOUNT {
            abort();
        }

        Self::from_inner(self.ptr)
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized, A: AllocRef> Deref for Arc<T, A> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner().data
    }
}

//#[unstable(feature = "receiver_trait", issue = "none")]
impl<T: ?Sized, A: AllocRef> Receiver for Arc<T, A> {}

impl<T: Clone, A: AllocRef + Clone + Sync> Arc<T, A> {
    /// Makes a mutable reference into the given `Arc`.
    ///
    /// If there are other `Arc` or [`Weak`] pointers to the same allocation,
    /// then `make_mut` will create a new allocation and invoke [`clone`][clone] on the inner value
    /// to ensure unique ownership. This is also referred to as clone-on-write.
    ///
    /// Note that this differs from the behavior of [`Rc::make_mut`] which disassociates
    /// any remaining `Weak` pointers.
    ///
    /// See also [`get_mut`][get_mut], which will fail rather than cloning.
    ///
    /// [clone]: Clone::clone
    /// [get_mut]: Arc::get_mut
    /// [`Rc::make_mut`]: super::rc::Rc::make_mut
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let mut data = Arc::new(5);
    ///
    /// *Arc::make_mut(&mut data) += 1;         // Won't clone anything
    /// let mut other_data = Arc::clone(&data); // Won't clone inner data
    /// *Arc::make_mut(&mut data) += 1;         // Clones inner data
    /// *Arc::make_mut(&mut data) += 1;         // Won't clone anything
    /// *Arc::make_mut(&mut other_data) *= 2;   // Won't clone anything
    ///
    /// // Now `data` and `other_data` point to different allocations.
    /// assert_eq!(*data, 8);
    /// assert_eq!(*other_data, 12);
    /// ```
    #[inline]
    //#[stable(feature = "arc_unique", since = "1.4.0")]
    pub fn make_mut(this: &mut Self) -> &mut T {
        // Note that we hold both a strong reference and a weak reference.
        // Thus, releasing our strong reference only will not, by itself, cause
        // the memory to be deallocated.
        //
        // Use Acquire to ensure that we see any writes to `weak` that happen
        // before release writes (i.e., decrements) to `strong`. Since we hold a
        // weak count, there's no chance the ArcInner itself could be
        // deallocated.
        if this.inner().strong.compare_exchange(1, 0, Acquire, Relaxed).is_err() {
            // Another strong pointer exists; clone
            let inner = this.inner();
            *this = Arc::new_in(inner.data.clone(), inner.alloc.clone());
        } else if this.inner().weak.load(Relaxed) != 1 {
            // Relaxed suffices in the above because this is fundamentally an
            // optimization: we are always racing with weak pointers being
            // dropped. Worst case, we end up allocated a new Arc unnecessarily.

            // We removed the last strong ref, but there are additional weak
            // refs remaining. We'll move the contents to a new Arc, and
            // invalidate the other weak refs.

            // Note that it is not possible for the read of `weak` to yield
            // usize::MAX (i.e., locked), since the weak count can only be
            // locked by a thread with a strong reference.

            // Materialize our own implicit weak pointer, so that it can clean
            // up the ArcInner as needed.
            let weak = Weak { ptr: this.ptr };

            // mark the data itself as already deallocated
            unsafe {
                // there is no data race in the implicit write caused by `read`
                // here (due to zeroing) because data is no longer accessed by
                // other threads (due to there being no more strong refs at this
                // point).
                let weak = weak.ptr.as_ref();
                let alloc = ptr::read(&weak.alloc);
                let data = ptr::read(&weak.data);
                let mut swap = Self::new_in(data, alloc);
                mem::swap(this, &mut swap);
                mem::forget(swap);
            }
        } else {
            // We were the sole reference of either kind; bump back up the
            // strong ref count.
            this.inner().strong.store(1, Release);
        }

        // As with `get_mut()`, the unsafety is ok because our reference was
        // either unique to begin with, or became one upon cloning the contents.
        unsafe { Self::get_mut_unchecked(this) }
    }
}

impl<T: ?Sized, A: AllocRef> Arc<T, A> {
    /// Returns a mutable reference into the given `Arc`, if there are
    /// no other `Arc` or [`Weak`] pointers to the same allocation.
    ///
    /// Returns [`None`] otherwise, because it is not safe to
    /// mutate a shared value.
    ///
    /// See also [`make_mut`][make_mut], which will [`clone`][clone]
    /// the inner value when there are other pointers.
    ///
    /// [make_mut]: Arc::make_mut
    /// [clone]: Clone::clone
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// let mut x = Arc::new(3);
    /// *Arc::get_mut(&mut x).unwrap() = 4;
    /// assert_eq!(*x, 4);
    ///
    /// let _y = Arc::clone(&x);
    /// assert!(Arc::get_mut(&mut x).is_none());
    /// ```
    #[inline]
    //#[stable(feature = "arc_unique", since = "1.4.0")]
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        if this.is_unique() {
            // This unsafety is ok because we're guaranteed that the pointer
            // returned is the *only* pointer that will ever be returned to T. Our
            // reference count is guaranteed to be 1 at this point, and we required
            // the Arc itself to be `mut`, so we're returning the only possible
            // reference to the inner data.
            unsafe { Some(Arc::get_mut_unchecked(this)) }
        } else {
            None
        }
    }

    /// Returns a mutable reference into the given `Arc`,
    /// without any check.
    ///
    /// See also [`get_mut`], which is safe and does appropriate checks.
    ///
    /// [`get_mut`]: Arc::get_mut
    ///
    /// # Safety
    ///
    /// Any other `Arc` or [`Weak`] pointers to the same allocation must not be dereferenced
    /// for the duration of the returned borrow.
    /// This is trivially the case if no such pointers exist,
    /// for example immediately after `Arc::new`.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(get_mut_unchecked)]
    ///
    /// use std::sync::Arc;
    ///
    /// let mut x = Arc::new(String::new());
    /// unsafe {
    ///     Arc::get_mut_unchecked(&mut x).push_str("foo")
    /// }
    /// assert_eq!(*x, "foo");
    /// ```
    #[inline]
    //#[unstable(feature = "get_mut_unchecked", issue = "63292")]
    pub unsafe fn get_mut_unchecked(this: &mut Self) -> &mut T {
        // We are careful to *not* create a reference covering the "count" fields, as
        // this would alias with concurrent access to the reference counts (e.g. by `Weak`).
        &mut (*this.ptr.as_ptr()).data
    }

    /// Determine whether this is the unique reference (including weak refs) to
    /// the underlying data.
    ///
    /// Note that this requires locking the weak ref count.
    fn is_unique(&mut self) -> bool {
        // lock the weak pointer count if we appear to be the sole weak pointer
        // holder.
        //
        // The acquire label here ensures a happens-before relationship with any
        // writes to `strong` (in particular in `Weak::upgrade`) prior to decrements
        // of the `weak` count (via `Weak::drop`, which uses release).  If the upgraded
        // weak ref was never dropped, the CAS here will fail so we do not care to synchronize.
        if self.inner().weak.compare_exchange(1, usize::MAX, Acquire, Relaxed).is_ok() {
            // This needs to be an `Acquire` to synchronize with the decrement of the `strong`
            // counter in `drop` -- the only access that happens when any but the last reference
            // is being dropped.
            let unique = self.inner().strong.load(Acquire) == 1;

            // The release write here synchronizes with a read in `downgrade`,
            // effectively preventing the above read of `strong` from happening
            // after the write.
            self.inner().weak.store(1, Release); // release the lock
            unique
        } else {
            false
        }
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
unsafe impl<#[may_dangle] T: ?Sized, A: AllocRef> Drop for Arc<T, A> {
    /// Drops the `Arc`.
    ///
    /// This will decrement the strong reference count. If the strong reference
    /// count reaches zero then the only other references (if any) are
    /// [`Weak`], so we `drop` the inner value.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// struct Foo;
    ///
    /// impl Drop for Foo {
    ///     fn drop(&mut self) {
    ///         println!("dropped!");
    ///     }
    /// }
    ///
    /// let foo  = Arc::new(Foo);
    /// let foo2 = Arc::clone(&foo);
    ///
    /// drop(foo);    // Doesn't print anything
    /// drop(foo2);   // Prints "dropped!"
    /// ```
    #[inline]
    fn drop(&mut self) {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object. This
        // same logic applies to the below `fetch_sub` to the `weak` count.
        if self.inner().strong.fetch_sub(1, Release) != 1 {
            return;
        }

        // This fence is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` fence. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this fence, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // In particular, while the contents of an Arc are usually immutable, it's
        // possible to have interior writes to something like a Mutex<T>. Since a
        // Mutex is not acquired when it is deleted, we can't rely on its
        // synchronization logic to make writes in thread A visible to a destructor
        // running in thread B.
        //
        // Also note that the Acquire fence here could probably be replaced with an
        // Acquire load, which could improve performance in highly-contended
        // situations. See [2].
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        // [2]: (https://github.com/rust-lang/rust/pull/41714)
        acquire!(self.inner().strong);

        unsafe {
            self.drop_slow();
        }
    }
}

impl<A: AllocRef> Arc<dyn Any + Send + Sync, A> {
    #[inline]
    //#[stable(feature = "rc_downcast", since = "1.29.0")]
    /// Attempt to downcast the `Arc<dyn Any + Send + Sync>` to a concrete type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::any::Any;
    /// use alloc_wg::sync::Arc;
    ///
    /// fn print_if_string(value: Arc<dyn Any + Send + Sync>) {
    ///     if let Ok(string) = value.downcast::<String>() {
    ///         println!("String ({}): {}", string.len(), string);
    ///     }
    /// }
    ///
    /// let my_string = "Hello World".to_string();
    /// print_if_string(Arc::new(my_string));
    /// print_if_string(Arc::new(0i8));
    /// ```
    pub fn downcast<T>(self) -> Result<Arc<T, A>, Self>
    where
        T: Any + Send + Sync + 'static,
    {
        if (*self).is::<T>() {
            let ptr = self.ptr.cast::<ArcInner<T, A>>();
            mem::forget(self);
            Ok(Arc::from_inner(ptr))
        } else {
            Err(self)
        }
    }
}

impl<T, A: AllocRef> Weak<T, A> {
    /// Constructs a new `Weak<T>`, without allocating any memory.
    /// Calling [`upgrade`] on the return value always gives [`None`].
    ///
    /// [`upgrade`]: Weak::upgrade
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Weak;
    ///
    /// let empty: Weak<i64> = Weak::new();
    /// assert!(empty.upgrade().is_none());
    /// ```
    //#[stable(feature = "downgraded_weak", since = "1.10.0")]
    pub fn new() -> Weak<T, A> {
        Weak { ptr: NonNull::new(usize::MAX as *mut ArcInner<T, A>).expect("MAX is not 0") }
    }

    /// Returns a raw pointer to the object `T` pointed to by this `Weak<T>`.
    ///
    /// The pointer is valid only if there are some strong references. The pointer may be dangling,
    /// unaligned or even [`null`] otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    /// use std::ptr;
    ///
    /// let strong = Arc::new("hello".to_owned());
    /// let weak = Arc::downgrade(&strong);
    /// // Both point to the same object
    /// assert!(ptr::eq(&*strong, weak.as_ptr()));
    /// // The strong here keeps it alive, so we can still access the object.
    /// assert_eq!("hello", unsafe { &*weak.as_ptr() });
    ///
    /// drop(strong);
    /// // But not any more. We can do weak.as_ptr(), but accessing the pointer would lead to
    /// // undefined behaviour.
    /// // assert_eq!("hello", unsafe { &*weak.as_ptr() });
    /// ```
    ///
    /// [`null`]: core::ptr::null
    //#[stable(feature = "weak_into_raw", since = "1.45.0")]
    pub fn as_ptr(&self) -> *const T {
        let ptr: *mut ArcInner<T, A> = NonNull::as_ptr(self.ptr);

        // SAFETY: we must offset the pointer manually, and said pointer may be
        // a dangling weak (usize::MAX) if T is sized. data_offset is safe to call,
        // because we know that a pointer to unsized T was derived from a real
        // unsized T, as dangling weaks are only created for sized T. wrapping_offset
        // is used so that we can use the same code path for the non-dangling
        // unsized case and the potentially dangling sized case.
        unsafe {
            let offset = data_offset::<_, A>(ptr as *mut T);
            set_data_ptr(ptr as *mut T, (ptr as *mut u8).wrapping_offset(offset))
        }
    }

    /// Consumes the `Weak<T>` and turns it into a raw pointer.
    ///
    /// This converts the weak pointer into a raw pointer, while still preserving the ownership of
    /// one weak reference (the weak count is not modified by this operation). It can be turned
    /// back into the `Weak<T>` with [`from_raw`].
    ///
    /// The same restrictions of accessing the target of the pointer as with
    /// [`as_ptr`] apply.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(allocator_api)]
    ///
    /// use alloc_wg::sync::{Arc, Weak};
    /// use std::alloc::Global;
    ///
    /// let strong = Arc::new("hello".to_owned());
    /// let weak = Arc::downgrade(&strong);
    /// let raw = weak.into_raw();
    ///
    /// assert_eq!(1, Arc::weak_count(&strong));
    /// assert_eq!("hello", unsafe { &*raw });
    ///
    /// drop(unsafe { <Weak<_, Global>>::from_raw(raw) });
    /// assert_eq!(0, Arc::weak_count(&strong));
    /// ```
    ///
    /// [`from_raw`]: Weak::from_raw
    /// [`as_ptr`]: Weak::as_ptr
    //#[stable(feature = "weak_into_raw", since = "1.45.0")]
    pub fn into_raw(self) -> *const T {
        let result = self.as_ptr();
        mem::forget(self);
        result
    }

    /// Converts a raw pointer previously created by [`into_raw`] back into `Weak<T>`.
    ///
    /// This can be used to safely get a strong reference (by calling [`upgrade`]
    /// later) or to deallocate the weak count by dropping the `Weak<T>`.
    ///
    /// It takes ownership of one weak reference (with the exception of pointers created by [`new`],
    /// as these don't own anything; the method still works on them).
    ///
    /// # Safety
    ///
    /// The pointer must have originated from the [`into_raw`] and must still own its potential
    /// weak reference.
    ///
    /// It is allowed for the strong count to be 0 at the time of calling this. Nevertheless, this
    /// takes ownership of one weak reference currently represented as a raw pointer (the weak
    /// count is not modified by this operation) and therefore it must be paired with a previous
    /// call to [`into_raw`].
    /// # Examples
    ///
    /// ```
    /// #![feature(allocator_api)]
    /// use alloc_wg::sync::{Arc, Weak};
    /// use std::alloc::Global;
    ///
    /// let strong = Arc::new("hello".to_owned());
    ///
    /// let raw_1 = Arc::downgrade(&strong).into_raw();
    /// let raw_2 = Arc::downgrade(&strong).into_raw();
    ///
    /// assert_eq!(2, Arc::weak_count(&strong));
    ///
    /// assert_eq!("hello", &*unsafe { <Weak<_, Global>>::from_raw(raw_1) }.upgrade().unwrap());
    /// assert_eq!(1, Arc::weak_count(&strong));
    ///
    /// drop(strong);
    ///
    /// // Decrement the last weak count.
    /// assert!(unsafe { <Weak<_, Global>>::from_raw(raw_2) }.upgrade().is_none());
    /// ```
    ///
    /// [`new`]: Weak::new
    /// [`into_raw`]: Weak::into_raw
    /// [`upgrade`]: Weak::upgrade
    /// [`forget`]: std::mem::forget
    //#[stable(feature = "weak_into_raw", since = "1.45.0")]
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        if ptr.is_null() {
            Self::new()
        } else {
            // See Arc::from_raw for details
            let offset = data_offset::<_, A>(ptr);
            let fake_ptr = ptr as *mut ArcInner<T, A>;
            let ptr = set_data_ptr(fake_ptr, (ptr as *mut u8).offset(-offset));
            Weak { ptr: NonNull::new(ptr).expect("Invalid pointer passed to from_raw") }
        }
    }
}

/// Helper type to allow accessing the reference counts without
/// making any assertions about the data field.
struct WeakInner<'a, A> {
    alloc: NonNull<A>,
    weak: &'a atomic::AtomicUsize,
    strong: &'a atomic::AtomicUsize,
}

impl<T: ?Sized, A: AllocRef> Weak<T, A> {
    /// Attempts to upgrade the `Weak` pointer to an [`Arc`], delaying
    /// dropping of the inner value if successful.
    ///
    /// Returns [`None`] if the inner value has since been dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// let weak_five = Arc::downgrade(&five);
    ///
    /// let strong_five: Option<Arc<_>> = weak_five.upgrade();
    /// assert!(strong_five.is_some());
    ///
    /// // Destroy all strong pointers.
    /// drop(strong_five);
    /// drop(five);
    ///
    /// assert!(weak_five.upgrade().is_none());
    /// ```
    //#[stable(feature = "arc_weak", since = "1.4.0")]
    pub fn upgrade(&self) -> Option<Arc<T, A>> {
        // We use a CAS loop to increment the strong count instead of a
        // fetch_add as this function should never take the reference count
        // from zero to one.
        let inner = self.inner()?;

        // Relaxed load because any write of 0 that we can observe
        // leaves the field in a permanently zero state (so a
        // "stale" read of 0 is fine), and any other value is
        // confirmed via the CAS below.
        let mut n = inner.strong.load(Relaxed);

        loop {
            if n == 0 {
                return None;
            }

            // See comments in `Arc::clone` for why we do this (for `mem::forget`).
            if n > MAX_REFCOUNT {
                abort();
            }

            // Relaxed is fine for the failure case because we don't have any expectations about the new state.
            // Acquire is necessary for the success case to synchronise with `Arc::new_cyclic`, when the inner
            // value can be initialized after `Weak` references have already been created. In that case, we
            // expect to observe the fully initialized value.
            match inner.strong.compare_exchange_weak(n, n + 1, Acquire, Relaxed) {
                Ok(_) => return Some(Arc::from_inner(self.ptr)), // null checked above
                Err(old) => n = old,
            }
        }
    }

    /// Gets the number of strong (`Arc`) pointers pointing to this allocation.
    ///
    /// If `self` was created using [`Weak::new`], this will return 0.
    //#[stable(feature = "weak_counts", since = "1.41.0")]
    pub fn strong_count(&self) -> usize {
        if let Some(inner) = self.inner() { inner.strong.load(SeqCst) } else { 0 }
    }

    /// Gets an approximation of the number of `Weak` pointers pointing to this
    /// allocation.
    ///
    /// If `self` was created using [`Weak::new`], or if there are no remaining
    /// strong pointers, this will return 0.
    ///
    /// # Accuracy
    ///
    /// Due to implementation details, the returned value can be off by 1 in
    /// either direction when other threads are manipulating any `Arc`s or
    /// `Weak`s pointing to the same allocation.
    //#[stable(feature = "weak_counts", since = "1.41.0")]
    pub fn weak_count(&self) -> usize {
        self.inner()
            .map(|inner| {
                let weak = inner.weak.load(SeqCst);
                let strong = inner.strong.load(SeqCst);
                if strong == 0 {
                    0
                } else {
                    // Since we observed that there was at least one strong pointer
                    // after reading the weak count, we know that the implicit weak
                    // reference (present whenever any strong references are alive)
                    // was still around when we observed the weak count, and can
                    // therefore safely subtract it.
                    weak - 1
                }
            })
            .unwrap_or(0)
    }

    #[inline]
    pub fn as_inner_ptr(&self) -> Option<NonNull<[u8]>> {
        if is_dangling(self.ptr) {
            None
        } else {
            Some({
                let ptr: NonNull<u8> = self.ptr.cast();
                let layout = unsafe { Layout::for_value_raw(self.ptr.as_ptr()) };
                NonNull::from(unsafe {
                    from_raw_parts(ptr.as_ptr(), layout.size())
                })
            })
        }
    }

    /// Gets the allocator used to allocate the original Arc for this weak ref. This alloc
    /// instance isn't dropped until the last Weak ref is dropped.
    /// Note that Weak::new() doesn't allocate, and so won't have an allocator instance to return
    /// here.
    #[inline]
    pub fn alloc_ref(&self) -> Option<&A>
        where A: Sync,
    {
        if is_dangling(self.ptr) {
            None
        } else {
            // We are careful to *not* create a reference covering the "data" field, as
            // the field may be mutated concurrently (for example, if the last `Arc`
            // is dropped, the data field will be dropped in-place).
            // The allocator instance will be valid as long as the last weak ref is still held,
            // so this is safe.
            Some(unsafe {
                let ptr = self.ptr.as_ptr();
                &(*ptr).alloc
            })
        }
    }

    /// Returns `None` when the pointer is dangling and there is no allocated `ArcInner`,
    /// (i.e., when this `Weak` was created by `Weak::new`).
    #[inline]
    fn inner(&self) -> Option<WeakInner<'_, A>> {
        if is_dangling(self.ptr) {
            None
        } else {
            // We are careful to *not* create a reference covering the "data" field, as
            // the field may be mutated concurrently (for example, if the last `Arc`
            // is dropped, the data field will be dropped in-place).
            Some(unsafe {
                let ptr = self.ptr.as_ptr();
                WeakInner {
                    alloc: NonNull::from(&(*ptr).alloc),
                    strong: &(*ptr).strong,
                    weak: &(*ptr).weak,
                }
            })
        }
    }

    /// Returns `true` if the two `Weak`s point to the same allocation (similar to
    /// [`ptr::eq`]), or if both don't point to any allocation
    /// (because they were created with `Weak::new()`).
    ///
    /// # Notes
    ///
    /// Since this compares pointers it means that `Weak::new()` will equal each
    /// other, even though they don't point to any allocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let first_rc = Arc::new(5);
    /// let first = Arc::downgrade(&first_rc);
    /// let second = Arc::downgrade(&first_rc);
    ///
    /// assert!(first.ptr_eq(&second));
    ///
    /// let third_rc = Arc::new(5);
    /// let third = Arc::downgrade(&third_rc);
    ///
    /// assert!(!first.ptr_eq(&third));
    /// ```
    ///
    /// Comparing `Weak::new`.
    ///
    /// ```
    /// use alloc_wg::sync::{Arc, Weak};
    ///
    /// let first = Weak::new();
    /// let second = Weak::new();
    /// assert!(first.ptr_eq(&second));
    ///
    /// let third_rc = Arc::new(());
    /// let third = Arc::downgrade(&third_rc);
    /// assert!(!first.ptr_eq(&third));
    /// ```
    ///
    /// [`ptr::eq`]: core::ptr::eq
    #[inline]
    //#[stable(feature = "weak_ptr_eq", since = "1.39.0")]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.ptr.as_ptr() == other.ptr.as_ptr()
    }

    /// Drops the `Weak` pointer, returning the allocator instance if this was the
    /// last reference.
    pub fn drop_alloc(self) -> Option<A> {
        // If we find out that we were the last weak pointer, then its time to
        // deallocate the data entirely. See the discussion in Arc::drop() about
        // the memory orderings
        //
        // It's not necessary to check for the locked state here, because the
        // weak count can only be locked if there was precisely one weak ref,
        // meaning that drop could only subsequently run ON that remaining weak
        // ref, which can only happen after the lock is released.
        let inner = if let Some(inner) = self.inner() { inner } else { return None; };

        if inner.weak.fetch_sub(1, Release) == 1 {
            acquire!(inner.weak);
            unsafe {
                let alloc = ptr::read(inner.alloc.as_ref());
                alloc.dealloc(self.ptr.cast(),
                              Layout::for_value(self.ptr.as_ref()));
                Some(alloc)
            }
        } else {
            None
        }
    }
}

//#[stable(feature = "arc_weak", since = "1.4.0")]
impl<T: ?Sized, A: AllocRef> Clone for Weak<T, A> {
    /// Makes a clone of the `Weak` pointer that points to the same allocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::{Arc, Weak};
    ///
    /// let weak_five = Arc::downgrade(&Arc::new(5));
    ///
    /// let _ = Weak::clone(&weak_five);
    /// ```
    #[inline]
    fn clone(&self) -> Weak<T, A> {
        let inner = if let Some(inner) = self.inner() {
            inner
        } else {
            return Weak { ptr: self.ptr };
        };
        // See comments in Arc::clone() for why this is relaxed.  This can use a
        // fetch_add (ignoring the lock) because the weak count is only locked
        // where are *no other* weak pointers in existence. (So we can't be
        // running this code in that case).
        let old_size = inner.weak.fetch_add(1, Relaxed);

        // See comments in Arc::clone() for why we do this (for mem::forget).
        if old_size > MAX_REFCOUNT {
            abort();
        }

        Weak { ptr: self.ptr }
    }
}

//#[stable(feature = "downgraded_weak", since = "1.10.0")]
impl<T, A: AllocRef> Default for Weak<T, A> {
    /// Constructs a new `Weak<T>`, without allocating memory.
    /// Calling [`upgrade`] on the return value always
    /// gives [`None`].
    ///
    /// [`upgrade`]: Weak::upgrade
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Weak;
    ///
    /// let empty: Weak<i64> = Default::default();
    /// assert!(empty.upgrade().is_none());
    /// ```
    fn default() -> Weak<T, A> {
        Weak::new()
    }
}

//#[stable(feature = "arc_weak", since = "1.4.0")]
impl<T: ?Sized, A: AllocRef> Drop for Weak<T, A> {
    /// Drops the `Weak` pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::{Arc, Weak};
    ///
    /// struct Foo;
    ///
    /// impl Drop for Foo {
    ///     fn drop(&mut self) {
    ///         println!("dropped!");
    ///     }
    /// }
    ///
    /// let foo = Arc::new(Foo);
    /// let weak_foo = Arc::downgrade(&foo);
    /// let other_weak_foo = Weak::clone(&weak_foo);
    ///
    /// drop(weak_foo);   // Doesn't print anything
    /// drop(foo);        // Prints "dropped!"
    ///
    /// assert!(other_weak_foo.upgrade().is_none());
    /// ```
    fn drop(&mut self) {
        // If we find out that we were the last weak pointer, then its time to
        // deallocate the data entirely. See the discussion in Arc::drop() about
        // the memory orderings
        //
        // It's not necessary to check for the locked state here, because the
        // weak count can only be locked if there was precisely one weak ref,
        // meaning that drop could only subsequently run ON that remaining weak
        // ref, which can only happen after the lock is released.
        let inner = if let Some(inner) = self.inner() { inner } else { return };

        if inner.weak.fetch_sub(1, Release) == 1 {
            acquire!(inner.weak);
            unsafe {
                let alloc = ptr::read(inner.alloc.as_ref());
                alloc.dealloc(self.ptr.cast(), Layout::for_value(self.ptr.as_ref()))
            }
        }
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
trait ArcEqIdent<T: ?Sized + PartialEq, A: AllocRef> {
    fn eq(&self, other: &Arc<T, A>) -> bool;
    fn ne(&self, other: &Arc<T, A>) -> bool;
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + PartialEq, A0: AllocRef, A1: AllocRef> ArcEqIdent<T, A1> for Arc<T, A0> {
    #[inline]
    default fn eq(&self, other: &Arc<T, A1>) -> bool {
        **self == **other
    }
    #[inline]
    default fn ne(&self, other: &Arc<T, A1>) -> bool {
        **self != **other
    }
}

/// We're doing this specialization here, and not as a more general optimization on `&T`, because it
/// would otherwise add a cost to all equality checks on refs. We assume that `Arc`s are used to
/// store large values, that are slow to clone, but also heavy to check for equality, causing this
/// cost to pay off more easily. It's also more likely to have two `Arc` clones, that point to
/// the same value, than two `&T`s.
///
/// We only do this when both allocators have the same type.
///
/// We can only do this when `T: Eq` as a `PartialEq` might be deliberately irreflexive.
//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + MarkerEq, A: AllocRef> ArcEqIdent<T, A> for Arc<T, A> {
    #[inline]
    fn eq(&self, other: &Arc<T, A>) -> bool {
        Arc::ptr_eq(self, other) || **self == **other
    }

    #[inline]
    fn ne(&self, other: &Arc<T, A>) -> bool {
        !Arc::ptr_eq(self, other) && **self != **other
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + PartialEq, A0: AllocRef, A1: AllocRef> PartialEq<Arc<T, A1>> for Arc<T, A0> {
    /// Equality for two `Arc`s.
    ///
    /// Two `Arc`s are equal if their inner values are equal, even if they are
    /// stored in different allocation.
    ///
    /// If `T` also implements `Eq` (implying reflexivity of equality),
    /// two `Arc`s that point to the same allocation are always equal.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert!(five == Arc::new(5));
    /// ```
    #[inline]
    fn eq(&self, other: &Arc<T, A1>) -> bool {
        ArcEqIdent::eq(self, other)
    }

    /// Inequality for two `Arc`s.
    ///
    /// Two `Arc`s are unequal if their inner values are unequal.
    ///
    /// If `T` also implements `Eq` (implying reflexivity of equality),
    /// two `Arc`s that point to the same value are never unequal.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert!(five != Arc::new(6));
    /// ```
    #[inline]
    fn ne(&self, other: &Arc<T, A1>) -> bool {
        ArcEqIdent::ne(self, other)
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + PartialOrd, A0: AllocRef, A1: AllocRef> PartialOrd<Arc<T, A1>> for Arc<T, A0> {
    /// Partial comparison for two `Arc`s.
    ///
    /// The two are compared by calling `partial_cmp()` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    /// use std::cmp::Ordering;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert_eq!(Some(Ordering::Less), five.partial_cmp(&Arc::new(6)));
    /// ```
    fn partial_cmp(&self, other: &Arc<T, A1>) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }

    /// Less-than comparison for two `Arc`s.
    ///
    /// The two are compared by calling `<` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert!(five < Arc::new(6));
    /// ```
    fn lt(&self, other: &Arc<T, A1>) -> bool {
        *(*self) < *(*other)
    }

    /// 'Less than or equal to' comparison for two `Arc`s.
    ///
    /// The two are compared by calling `<=` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert!(five <= Arc::new(5));
    /// ```
    fn le(&self, other: &Arc<T, A1>) -> bool {
        *(*self) <= *(*other)
    }

    /// Greater-than comparison for two `Arc`s.
    ///
    /// The two are compared by calling `>` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert!(five > Arc::new(4));
    /// ```
    fn gt(&self, other: &Arc<T, A1>) -> bool {
        *(*self) > *(*other)
    }

    /// 'Greater than or equal to' comparison for two `Arc`s.
    ///
    /// The two are compared by calling `>=` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert!(five >= Arc::new(5));
    /// ```
    fn ge(&self, other: &Arc<T, A1>) -> bool {
        *(*self) >= *(*other)
    }
}
//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + Ord, A: AllocRef> Ord for Arc<T, A> {
    /// Comparison for two `Arc`s.
    ///
    /// The two are compared by calling `cmp()` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    /// use std::cmp::Ordering;
    ///
    /// let five = Arc::new(5);
    ///
    /// assert_eq!(Ordering::Less, five.cmp(&Arc::new(6)));
    /// ```
    fn cmp(&self, other: &Arc<T, A>) -> Ordering {
        (**self).cmp(&**other)
    }
}
//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + Eq, A: AllocRef> Eq for Arc<T, A> {}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + fmt::Display, A: AllocRef> fmt::Display for Arc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + fmt::Debug, A: AllocRef> fmt::Debug for Arc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized, A: AllocRef> fmt::Pointer for Arc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&(&**self as *const T), f)
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: Default, A: AllocRef + Default> Default for Arc<T, A> {
    /// Creates a new `Arc<T>`, with the `Default` value for `T`.
    ///
    /// # Examples
    ///
    /// ```
    /// use alloc_wg::sync::Arc;
    ///
    /// let x: Arc<i32> = Default::default();
    /// assert_eq!(*x, 0);
    /// ```
    fn default() -> Arc<T, A> {
        Arc::new_in(Default::default(), A::default())
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized + Hash, A: AllocRef> Hash for Arc<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

//#[stable(feature = "from_for_ptrs", since = "1.6.0")]
impl<T> From<T> for Arc<T> {
    fn from(t: T) -> Self {
        Arc::new(t)
    }
}

//#[stable(feature = "shared_from_slice", since = "1.21.0")]
impl<T: Clone> From<&[T]> for Arc<[T]> {
    #[inline]
    fn from(v: &[T]) -> Arc<[T]> {
        <Self as ArcFromSlice<T>>::from_slice(v)
    }
}

//#[stable(feature = "shared_from_slice", since = "1.21.0")]
impl From<&str> for Arc<str> {
    #[inline]
    fn from(v: &str) -> Arc<str> {
        let arc = Arc::<[u8]>::from(v.as_bytes());
        unsafe { Arc::from_raw(Arc::into_raw(arc) as *const str) }
    }
}

//#[stable(feature = "shared_from_slice", since = "1.21.0")]
impl From<std::string::String> for Arc<str> {
    #[inline]
    fn from(v: std::string::String) -> Arc<str> {
        Arc::from(&v[..])
    }
}
impl<A: AllocRef> From<String<A>> for Arc<str, A> {
    #[inline]
    fn from(v: String<A>) -> Arc<str, A> {
        unsafe {
            let (mem, slice, alloc) = v.leak_alloc();
            let mut arc = Arc::copy_from_str(slice, alloc);

            if let Some(mem) = mem {
                let layout = Layout::from_size_align_unchecked(mem.as_ref().len(),
                                                               align_of::<u8>());
                // this Arc can't be shared yet, so this is safe.
                arc.inner_mut().alloc.dealloc(mem.cast(), layout);
            }

            arc
        }
    }
}

//#[stable(feature = "shared_from_slice", since = "1.21.0")]
impl<T: ?Sized, A: AllocRef> From<Box<T, A>> for Arc<T, A> {
    #[inline]
    fn from(v: Box<T, A>) -> Arc<T, A> {
        Arc::from_box(v)
    }
}

//#[stable(feature = "shared_from_slice", since = "1.21.0")]
impl<T, A: AllocRef> From<Vec<T, A>> for Arc<[T], A> {
    #[inline]
    fn from(v: Vec<T, A>) -> Arc<[T], A> {
        unsafe {
            let (mem, slice, alloc) = v.leak_alloc();
            let mut arc = Arc::copy_from_slice(slice, alloc);

            if let Some(mem) = mem {
                let layout = Layout::from_size_align_unchecked(mem.as_ref().len(),
                                                               align_of::<T>());
                // this Arc can't be shared yet, so this is safe.
                arc.inner_mut().alloc.dealloc(mem.cast(), layout);
            }

            arc
        }
    }
}

//#[stable(feature = "shared_from_cow", since = "1.45.0")]
impl<'a, B> From<Cow<'a, B>> for Arc<B>
where
    B: ToOwned + ?Sized,
    Arc<B>: From<&'a B> + From<B::Owned>,
{
    #[inline]
    fn from(cow: Cow<'a, B>) -> Arc<B> {
        match cow {
            Cow::Borrowed(s) => Arc::from(s),
            Cow::Owned(s) => Arc::from(s),
        }
    }
}

//#[stable(feature = "boxed_slice_try_from", since = "1.43.0")]
impl<T, A: AllocRef, const N: usize> TryFrom<Arc<[T], A>> for Arc<[T; N], A> {
    type Error = Arc<[T], A>;

    fn try_from(boxed_slice: Arc<[T], A>) -> Result<Self, Self::Error> {
        if boxed_slice.len() == N {
            Ok(unsafe { Arc::from_raw(Arc::into_raw(boxed_slice) as *mut [T; N]) })
        } else {
            Err(boxed_slice)
        }
    }
}
/*
// error[E0520]: `Error` specializes an item from a parent `impl`, but that item is not marked
// `default`?
// What. No.
impl<T, A: AllocRef> TryInto<Arc<[T], A>> for Vec<T, A> {
    type Error = (TryReserveError, Vec<T, A>);

    fn try_into(self) -> Result<Arc<[T], A>, Self::Error> {
        unsafe {
            let capacity = self.capacity();
            let (mem, slice, alloc) = self.leak_alloc();
            let mut arc = Arc::try_copy_from_slice(slice, alloc)
              .map_err(|(err, alloc)| {
                  let v = Vec::from_raw_parts_in(slice.as_mut_ptr(),
                                                 slice.len(), capacity,
                                                 alloc);
                  (err, v)
              })?;

            if let Some(mem) = mem {
                let layout = Layout::from_size_align_unchecked(mem.as_ref().len(),
                                                               align_of::<T>());
                // this Arc can't be shared yet, so this is safe.
                arc.inner_mut().alloc.dealloc(mem.cast(), layout);
            }

            Ok(arc)
        }
    }
}
*/

//#[stable(feature = "shared_from_iter", since = "1.37.0")]
impl<T> iter::FromIterator<T> for Arc<[T]> {
    /// Takes each element in the `Iterator` and collects it into an `Arc<[T]>`.
    ///
    /// # Performance characteristics
    ///
    /// ## The general case
    ///
    /// In the general case, collecting into `Arc<[T]>` is done by first
    /// collecting into a `Vec<T>`. That is, when writing the following:
    ///
    /// ```rust
    /// # use alloc_wg::sync::Arc;
    /// let evens: Arc<[u8]> = (0..10).filter(|&x| x % 2 == 0).collect();
    /// # assert_eq!(&*evens, &[0, 2, 4, 6, 8]);
    /// ```
    ///
    /// this behaves as if we wrote:
    ///
    /// ```rust
    /// # use alloc_wg::{sync::Arc, vec::Vec};
    /// let evens: Arc<[u8]> = (0..10).filter(|&x| x % 2 == 0)
    ///     .collect::<Vec<_>>() // The first set of allocations happens here.
    ///     .into(); // A second allocation for `Arc<[T]>` happens here.
    /// # assert_eq!(&*evens, &[0, 2, 4, 6, 8]);
    /// ```
    ///
    /// This will allocate as many times as needed for constructing the `Vec<T>`
    /// and then it will allocate once for turning the `Vec<T>` into the `Arc<[T]>`.
    ///
    /// ## Iterators of known length
    ///
    /// When your `Iterator` implements `TrustedLen` and is of an exact size,
    /// a single allocation will be made for the `Arc<[T]>`. For example:
    ///
    /// ```rust
    /// # use alloc_wg::{sync::Arc, vec::Vec};
    /// let evens: Arc<[u8]> = (0..10).collect(); // Just a single allocation happens here.
    /// # assert_eq!(&*evens, &*(0..10).collect::<Vec<_>>());
    /// ```
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        ToArcSlice::to_arc_slice(iter.into_iter())
    }
}
impl<T, A: AllocRef> FromIteratorIn<T, A> for Arc<[T], A> {
    #[inline]
    #[must_use]
    fn from_iter_in<I: IntoIterator<Item = T>>(iter: I, a: A) -> Self {
        <Vec<T, A> as SpecExtend<T, I::IntoIter, A>>::from_iter_in(iter.into_iter(), a)
          .into()
    }

    #[inline]
    fn try_from_iter_in<I: IntoIterator<Item = T>>(iter: I, a: A) -> Result<Self, TryReserveError> {
        let iter = iter.into_iter();
        let v = <Vec<T, A> as SpecExtend<T, I::IntoIter, A>>::try_from_iter_in(iter, a)?;
        unsafe {
            let (mem, slice, alloc) = v.leak_alloc();
            let mut arc = Arc::try_copy_from_slice(slice, alloc)
              .map_err(map_error)?;

            if let Some(mem) = mem {
                let layout = Layout::from_size_align_unchecked(mem.as_ref().len(),
                                                               align_of::<T>());
                // this Arc can't be shared yet, so this is safe.
                arc.inner_mut().alloc.dealloc(mem.cast(), layout);
            }

            Ok(arc)
        }
    }
}

/// Specialization trait used for collecting into `Arc<[T]>`.
trait ToArcSlice<T>: Iterator<Item = T> + Sized {
    fn to_arc_slice(self) -> Arc<[T]>;
}

impl<T, I: Iterator<Item = T>> ToArcSlice<T> for I {
    default fn to_arc_slice(self) -> Arc<[T]> {
        self.collect::<Vec<T>>().into()
    }
}

impl<T, I: iter::TrustedLen<Item = T>> ToArcSlice<T> for I {
    fn to_arc_slice(self) -> Arc<[T]> {
        // This is the case for a `TrustedLen` iterator.
        let (low, high) = self.size_hint();
        if let Some(high) = high {
            debug_assert_eq!(
                low,
                high,
                "TrustedLen iterator's size hint is not exact: {:?}",
                (low, high)
            );

            unsafe {
                // SAFETY: We need to ensure that the iterator has an exact length and we have.
                Arc::from_iter_exact(self, low, Default::default())
            }
        } else {
            // Fall back to normal implementation.
            self.collect::<Vec<T>>().into()
        }
    }
}

//#[stable(feature = "rust1", since = "1.0.0")]
impl<T: ?Sized, A: AllocRef> borrow::Borrow<T> for Arc<T, A> {
    fn borrow(&self) -> &T {
        &**self
    }
}

//#[stable(since = "1.5.0", feature = "smart_ptr_as_ref")]
impl<T: ?Sized, A: AllocRef> AsRef<T> for Arc<T, A> {
    fn as_ref(&self) -> &T {
        &**self
    }
}

//#[stable(feature = "pin", since = "1.33.0")]
impl<T: ?Sized, A: AllocRef> Unpin for Arc<T, A> {}

/// Get the offset within an `ArcInner` for
/// a payload of type described by a pointer.
///
/// # Safety
///
/// This has the same safety requirements as `align_of_val_raw`. In effect:
///
/// - This function is safe for any argument if `T` is sized, and
/// - if `T` is unsized, the pointer must have appropriate pointer metadata
///   acquired from the real instance that you are getting this offset for.
unsafe fn data_offset<T: ?Sized, A: AllocRef>(ptr: *const T) -> isize {
    // Align the unsized value to the end of the `ArcInner`.
    // Because it is `?Sized`, it will always be the last field in memory.
    // Note: This is a detail of the current implementation of the compiler,
    // and is not a guaranteed language detail. Do not rely on it outside of std.
    data_offset_align::<A>(align_of_val(&*ptr))
}

#[inline]
fn data_offset_align<A: AllocRef>(align: usize) -> isize {
    let layout = Layout::new::<ArcInner<(), A>>();
    (layout.size() + layout.padding_needed_for(align)) as isize
}

#[inline]
fn is_dangling<T: ?Sized>(ptr: NonNull<T>) -> bool {
    let address = ptr.as_ptr() as *mut () as usize;
    address == usize::MAX
}
#[inline]
unsafe fn box_free<T: ?Sized>(ptr: Unique<T>) {
    let size = size_of_val(ptr.as_ref());
    let align = align_of_val(ptr.as_ref());
    let layout = Layout::from_size_align_unchecked(size, align);
    Global.dealloc(ptr.cast().into(), layout)
}

#[inline(always)]
fn map_error<A>((err, _): (TryReserveError, A)) -> TryReserveError { err }

// Hack to allow specializing on `Eq` even though `Eq` has a method.
//#[rustc_unsafe_specialization_marker]
pub(crate) trait MarkerEq: PartialEq<Self> {}

impl<T: Eq> MarkerEq for T {}
