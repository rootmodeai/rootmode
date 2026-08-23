//! rootmode worker — what you run on the box with the GPUs.
//!
//! It advertises what the node can actually do, accepts jobs over
//! RootmodeProtocol v1, and drives a local inference backend (vLLM for text,
//! ComfyUI for images). It holds no models of its own and executes nothing a
//! client sends: a job selects a declared workflow and fills declared
//! parameters, full stop.
//!
//! It can be reached two ways, with identical behaviour on both: a direct
//! `ws://host:port` address someone typed, or the peer-to-peer network, where
//! it announces what it serves and clients discover it. See [`p2p`].

pub mod backends;
pub mod chain;
pub mod channels;
pub mod config;
pub mod error;
pub mod p2p;
pub mod screen;
pub mod server;
pub mod stats;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

pub use config::Config;
pub use error::{Result, WorkerError};
pub use server::Worker;
