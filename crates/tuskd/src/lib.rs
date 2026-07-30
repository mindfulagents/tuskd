#![forbid(unsafe_code)]

//! tuskd: the `tuskd` binary — CLI, daemon, transports.

pub mod admin;
pub mod archive;
pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod dashboard;
pub mod mcp_protocol;
pub mod platform;
pub mod runtime;
pub mod setup;
pub mod stdio;
pub mod sync;
pub mod sync_cloud;
