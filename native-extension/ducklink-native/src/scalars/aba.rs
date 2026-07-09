//! `aba_*` scalars — US ABA routing-number checks. Wraps `aba_core::logic`.

use super::helpers::{BoolScalar, IntScalar, TextScalar};
use aba_core::logic;

pub struct Validate;
impl BoolScalar for Validate {
    const NAME: &'static str = "aba_validate";
    fn invoke(text: &str) -> bool {
        logic::validate(text)
    }
}

pub struct FrbDistrict;
impl IntScalar for FrbDistrict {
    const NAME: &'static str = "aba_frb_district";
    fn invoke(text: &str) -> Option<i64> {
        logic::frb(text).map(|d| d as i64)
    }
}

pub struct FedRegion;
impl TextScalar for FedRegion {
    const NAME: &'static str = "aba_fed_region";
    fn invoke(text: &str) -> Option<String> {
        logic::frb(text).map(|d| logic::fed_region(d).to_string())
    }
}
