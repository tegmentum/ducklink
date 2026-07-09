//! `cc_*` scalars — credit-card validation + brand/masking utilities.
//! Wraps `creditcard_core::logic`.

use super::helpers::{BoolScalar, TextScalar};
use creditcard_core::logic;

pub struct Validate;
impl BoolScalar for Validate {
    const NAME: &'static str = "cc_validate";
    fn invoke(text: &str) -> bool {
        logic::digits(text).map(|d| logic::validate(&d)).unwrap_or(false)
    }
}

pub struct Network;
impl TextScalar for Network {
    const NAME: &'static str = "cc_network";
    fn invoke(text: &str) -> Option<String> {
        logic::digits(text)
            .and_then(|d| logic::network(&d))
            .map(|n| n.to_string())
    }
}

pub struct Type;
impl TextScalar for Type {
    const NAME: &'static str = "cc_type";
    fn invoke(text: &str) -> Option<String> {
        let d = logic::digits_only(text);
        logic::brand(&d).map(|t| t.to_string())
    }
}

pub struct Mask;
impl TextScalar for Mask {
    const NAME: &'static str = "cc_mask";
    fn invoke(text: &str) -> Option<String> {
        let d = logic::digits_only(text);
        if d.is_empty() {
            None
        } else {
            Some(logic::mask(&d))
        }
    }
}

pub struct Last4;
impl TextScalar for Last4 {
    const NAME: &'static str = "cc_last4";
    fn invoke(text: &str) -> Option<String> {
        let d = logic::digits_only(text);
        if d.len() >= 4 {
            Some(d[d.len() - 4..].to_string())
        } else {
            None
        }
    }
}

pub struct Bin;
impl TextScalar for Bin {
    const NAME: &'static str = "cc_bin";
    fn invoke(text: &str) -> Option<String> {
        let d = logic::digits_only(text);
        if d.len() >= 6 {
            Some(d[..6].to_string())
        } else {
            None
        }
    }
}

pub struct Normalize;
impl TextScalar for Normalize {
    const NAME: &'static str = "cc_normalize";
    fn invoke(text: &str) -> Option<String> {
        let d = logic::digits_only(text);
        if d.is_empty() {
            None
        } else {
            Some(d)
        }
    }
}
