//! The receiver's integration tests, in one binary (each test binary links the whole receiver,
//! and the build machine is short on disk).

mod common;

mod agent;
mod cli;
mod deploy;
mod e2e;
mod handle;
mod itc;
mod link;
mod transcribe;
mod web;
