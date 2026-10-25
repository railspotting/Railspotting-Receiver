//! The RailSpotting receiver binary's library: the agent, dongle threads and librtlsdr (the USB
//! side), the speech side of transcription and the status page.

// the agent has long argument lists and large error and return types
#![allow(clippy::too_many_arguments, clippy::result_large_err, clippy::type_complexity)]

pub mod agent;
pub mod cli;
pub mod device;
pub mod handle;
pub mod itc;
pub mod logging;
pub mod models;
pub mod rtl;
pub mod transcribe;
pub mod util;
pub mod wav;
pub mod web;
#[cfg(feature = "speech")]
pub mod whisper;

pub use agent::{Agent, AgentOptions, AgentTranscriber};
