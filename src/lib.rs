//! Functional Reactive Programming library for Rust
#![warn(missing_docs)]

#[macro_use]
mod helpers;
pub mod lift;
pub mod signal;
pub mod stream;
pub mod types;

pub use crate::signal::Signal;
pub use crate::stream::{Sink, Stream};
