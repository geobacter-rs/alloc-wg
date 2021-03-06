//! An attempt to collect several proposals of [rust-lang/wg-allocators](https://github.com/rust-lang/wg-allocators) into a
//! MVP.
//!
//! [`Alloc`]: https://doc.rust-lang.org/1.38.0/alloc/alloc/trait.Alloc.html
//! [`AllocRef`]: crate::alloc::AllocRef
//! [`AllocRef::alloc`]: crate::alloc::AllocRef::alloc
//! [`AllocRef::alloc_zeroed`]: crate::alloc::AllocRef::alloc_zeroed
//! [`Box`]: crate::boxed::Box
//! [`DeallocRef`]: crate::alloc::DeallocRef
//! [`ReallocRef`]: crate::alloc::ReallocRef
//! [`BuildAllocRef`]: crate::alloc::BuildAllocRef
//! [`BuildHasher`]: https://doc.rust-lang.org/1.38.0/core/hash/trait.BuildHasher.html
//! [`Hasher`]: https://doc.rust-lang.org/1.38.0/core/hash/trait.Hasher.html
//! [`NonZeroLayout`]: crate::alloc::NonZeroLayout

#![feature(
    allocator_api,
    alloc_layout_extra,
    cfg_sanitize,
    coerce_unsized,
    const_alloc_layout,
    const_fn,
    const_generics,
    const_panic,
    const_raw_ptr_to_usize_cast,
    core_intrinsics,
    dispatch_from_dyn,
    dropck_eyepatch,
    exact_size_is_empty,
    exclusive_range_pattern,
    exhaustive_patterns,
    extend_one,
    fn_traits,
    maybe_uninit_extra,
    maybe_uninit_ref,
    maybe_uninit_slice,
    maybe_uninit_uninit_array,
    never_type,
    or_patterns,
    ptr_internals,
    raw_ref_op,
    raw_vec_internals,
    receiver_trait,
    slice_ptr_get,
    slice_ptr_len,
    specialization,
    str_internals,
    structural_match,
    trusted_len,
    unboxed_closures,
    unsafe_block_in_unsafe_fn,
    unsize,
    unsized_locals
)]
#![feature(btree_drain_filter)]
#![feature(map_first_last)]
#![feature(layout_for_ptr)]
#![cfg_attr(not(feature = "std"), no_std)]
#![doc(test(attr(
    deny(
        future_incompatible,
        macro_use_extern_crate,
        nonstandard_style,
        rust_2018_compatibility,
        rust_2018_idioms,
        trivial_casts,
        trivial_numeric_casts,
        unused_import_braces,
        unused_lifetimes,
        unused_qualifications,
        variant_size_differences,
    ),
    allow(unused_extern_crates)
)))]
#![warn(
    future_incompatible,
    macro_use_extern_crate,
    nonstandard_style,
    rust_2018_compatibility,
    rust_2018_idioms,
    single_use_lifetimes,
    trivial_numeric_casts,
    unused,
    unused_import_braces,
    unused_lifetimes,
)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_safety_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    incomplete_features
)]

#[macro_export]
macro_rules! vec {
    ($($x:expr),*) => ({
        let mut v = $crate::vec::Vec::new();
        $( v.push($x); )*
        v
    });
    ($($x:expr,)*) => ($crate::vec![$($x),*]);
    ($elem:expr; $n:expr) => (
        $crate::vec::from_elem($elem, $n)
    );
    (in $alloc:expr) => {
        $crate::vec::Vec::new_in($alloc)
    };
    (in $alloc:expr; $($x:expr),*) => {{
        let mut v = $crate::vec::Vec::new_in($alloc);
        $( v.push($x); )*
        v
    }};
    (in $alloc:expr; $($x:expr,)*) => ($crate::vec![in $alloc; $($x),*]);
    (in $alloc:expr; $elem:expr; $n:expr) => {{
        $crate::vec::from_elem_in($elem, $n, $alloc)
    }};
    (try $($x:expr),*) => {{
        (|| -> Result<$crate::vec::Vec<_,_>, $crate::collections::CollectionAllocErr<_>> {
            let mut v = $crate::vec::Vec::new();
            $( v.try_push($x)?; )*
            Ok(v)
        })()
    }};
    (try $($x:expr,)*) => ($crate::vec![try $($x),*]);
    (try $elem:expr; $n:expr) => {{
        $crate::vec::try_from_elem_in($elem, $n, $crate::alloc::AbortAlloc<$crate::alloc::Global>)
    }};
    (try in $alloc:expr; $($x:expr),*) => {{
        (|| -> Result<$crate::vec::Vec<_,_>, $crate::collections::CollectionAllocErr<_>> {
            let mut v = $crate::vec::Vec::new_in($alloc);
            $( v.try_push($x)?; )*
            Ok(v)
        })()
    }};
    (try in $alloc:expr; $($x:expr,)*) => ($crate::vec![try in $alloc; $($x),*]);
    (try in $alloc:expr; $elem:expr; $n:expr) => {{
        $crate::vec::try_from_elem_in($elem, $n, $alloc)
    }};
}

#[macro_export]
macro_rules! format {
    ( in $alloc:expr, $fmt:expr, $($args:expr),* ) => {{
        use std::fmt::Write;
        let mut s = $crate::string::String::new_in($alloc);
        let _ = write!(&mut s, $fmt, $($args),*);
        s
    }};
    ( $fmt:expr, $($args:expr),* ) => {{
        use std::fmt::Write;
        let mut s = $crate::string::String::new();
        let _ = write!(&mut s, $fmt, $($args),*);
        s
    }}
}

// pub mod alloc;
pub use liballoc::alloc;
pub mod boxed;
mod btree;
pub mod clone;
pub mod collections;
pub mod iter;
pub mod raw_vec;
pub mod str;
pub mod string;
pub mod sync;
pub mod vec;

extern crate alloc as liballoc;

pub use liballoc::{borrow, fmt, rc, slice};

use crate::collections::TryReserveError;
use liballoc::alloc::handle_alloc_error;

// One central function responsible for reporting capacity overflows. This'll
// ensure that the code generation related to these panics is minimal as there's
// only one location which panics rather than a bunch throughout the module.
pub(in crate) fn capacity_overflow() -> ! {
    panic!("capacity overflow");
}

pub(crate) fn handle_reserve_error<T>(result: Result<T, TryReserveError>) -> T {
    match result {
        Ok(t) => t,
        Err(TryReserveError::AllocError { layout }) => handle_alloc_error(layout),
        Err(TryReserveError::CapacityOverflow) => capacity_overflow(),
    }
}
