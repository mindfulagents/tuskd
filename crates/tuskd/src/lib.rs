#![forbid(unsafe_code)]

//! tuskd: the `opentusk` binary — CLI, daemon, transports.

pub mod admin;
pub mod archive;
pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod mcp_protocol;
pub mod platform;
pub mod runtime;
pub mod stdio;
