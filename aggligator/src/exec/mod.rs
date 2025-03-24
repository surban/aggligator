//! Async executive for futures.
//!
//! On native platforms this uses Tokio.
//! On JavaScript this executes Futures as Promises.

#[cfg(not(feature = "js"))]
mod native;

#[cfg(not(feature = "js"))]
pub use native::*;

#[cfg(feature = "js")]
mod js;

#[cfg(feature = "js")]
pub use js::*;

pub use task::spawn;
