//! All native scalars in the bundle, one submodule per source `-core` crate.
//!
//! Each submodule exposes zero-sized types implementing [`helpers::BoolScalar`],
//! [`helpers::TextScalar`], or [`helpers::IntScalar`]. `lib.rs` iterates
//! through them at load time and registers each via the appropriate adapter.

pub mod helpers;

pub mod aba;
pub mod creditcard;
pub mod iban;
pub mod isbn;
pub mod luhn;
