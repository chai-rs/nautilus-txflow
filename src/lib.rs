//! TxFlow perpetuals exchange adapter for NautilusTrader.
//!
//! Public market data works with no credentials. Trading needs an account address and an
//! approved agent key, read from the configuration or from environment variables at run time.

#![warn(rustc::all)]
#![deny(unsafe_code)]
#![deny(nonstandard_style)]
#![deny(missing_debug_implementations)]
#![deny(clippy::missing_panics_doc)]
#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::clone_on_copy)]

pub mod book;
pub mod common;
pub mod config;
pub mod data;
pub mod execution;
pub mod factories;
pub mod fuzz;
pub mod http;
#[cfg(feature = "python")]
pub mod python;
pub mod signing;
pub mod websocket;
