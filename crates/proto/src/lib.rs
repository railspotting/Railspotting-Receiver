//! The headend protocol: device keys and signing, the signed API client (contract v1.4),
//! device linking codes, and (feature `fake`) an in-process headend for tests.

// ApiError carries the reply body, and request() takes every option as an argument.
#![allow(clippy::result_large_err, clippy::too_many_arguments)]

pub mod api;
pub mod keys;
pub mod link;
pub mod jsonfmt;

#[cfg(feature = "fake")]
pub mod fake;

pub use api::{dumps, ApiClient, ApiError, Backoff, Response};
