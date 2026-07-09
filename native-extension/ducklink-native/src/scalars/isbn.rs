//! `isbn_*` scalars — International Standard Book Number checks.
//! Wraps `isbn_core::logic`.

use super::helpers::{BoolScalar, TextScalar};
use isbn_core::logic;

pub struct Valid;
impl BoolScalar for Valid {
    const NAME: &'static str = "isbn_valid";
    fn invoke(text: &str) -> bool {
        // Match the WASM declare!() body byte-for-byte.
        logic::is_valid(&logic::clean(text))
    }
}

pub struct Normalize;
impl TextScalar for Normalize {
    const NAME: &'static str = "isbn_normalize";
    fn invoke(text: &str) -> Option<String> {
        let body = logic::clean(text);
        if logic::is_valid(&body) {
            Some(body)
        } else {
            None
        }
    }
}
