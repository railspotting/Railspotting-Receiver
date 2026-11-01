//! ITC (Interoperable Train Control) decoding for the RailSpotting receiver: the 220 MHz radio
//! system that carries positive train control (PTC) messages. Receive only.
//!
//! - `dsp`: the channel plan, a channelizer, burst finding and pi/4-DQPSK demodulation;
//! - `link`: unique word, convolutional header, RS(16,12) de-interleaving and correction (with
//!   erasures from soft decisions), CRC-16;
//! - `msg`: the fields of the message types that are understood;
//! - `decode`: samples in, decoded messages out (`messages.md` describes them);
//! - `recording`: `.cu8` recordings with a `.json` sidecar;
//! - `feed`: decoded messages over TCP for a monitor;
//! - `survey`: the tally and report of `railspotting-receiver itc survey`;
//! - `changes`: wayside state changes and base stations, as records for the headend (contract
//!   v1.18);
//! - `place`: where wayside units are, from train timing, a defect detector and mapped signals,
//!   as proposals for the headend (contract v1.18 section 7).
//!
//! How the protocol was worked out, with the evidence, is in `findings.md`. Decoded messages
//! describe railroad operations: they stay on the receiver (its log and local feed). Only state
//! changes and proposed positions go to the headend, and only when its owner turns ITC upload on.

pub mod changes;
pub mod decode;
pub mod dsp;
pub mod feed;
pub mod link;
pub mod msg;
pub mod place;
pub mod recording;
pub mod survey;
