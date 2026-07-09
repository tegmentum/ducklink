//! `luhn_*` scalars — Luhn checksum (credit-card check-digit family).
//! Wraps `luhn_core::logic`.

use super::helpers::{BoolScalar, IntScalar};
use luhn_core::logic;

pub struct Validate;
impl BoolScalar for Validate {
    const NAME: &'static str = "luhn_validate";
    fn invoke(text: &str) -> bool {
        logic::digits(text).map(|d| logic::validate(&d)).unwrap_or(false)
    }
}

pub struct CheckDigit;
impl IntScalar for CheckDigit {
    const NAME: &'static str = "luhn_check_digit";
    fn invoke(text: &str) -> Option<i64> {
        logic::digits(text).map(|d| logic::check_digit(&d) as i64)
    }
}
