//! Library surface of `littleguy`, exposing [`mandate_bridge`], [`orchestration`], [`pool`], [`rule_of_two`], and [`secret_file`] alongside the `lg` daemon binary.

pub mod mandate_bridge;
pub mod pool;
pub mod secret_file;
pub use littleguy_core::orchestration;
pub use littleguy_core::rule_of_two;
