//! `iban_*` scalars — international bank account number checks.
//! Wraps `iban_core::logic`.

use super::helpers::{BoolScalar, TextScalar};
use iban_core::logic;

pub struct Validate;
impl BoolScalar for Validate {
    const NAME: &'static str = "iban_validate";
    fn invoke(text: &str) -> bool {
        logic::validate(text)
    }
}

pub struct Country;
impl TextScalar for Country {
    const NAME: &'static str = "iban_country";
    fn invoke(text: &str) -> Option<String> {
        logic::country(text)
    }
}

pub struct Bban;
impl TextScalar for Bban {
    const NAME: &'static str = "iban_bban";
    fn invoke(text: &str) -> Option<String> {
        logic::bban(text)
    }
}
