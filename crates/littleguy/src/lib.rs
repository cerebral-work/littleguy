//! Library surface of `littleguy`, exposing [`mandate_bridge`], [`orchestration`], and [`rule_of_two`] alongside the `lg` daemon binary.

pub mod mandate_bridge;
pub use littleguy_core::orchestration;
pub use littleguy_core::rule_of_two;
