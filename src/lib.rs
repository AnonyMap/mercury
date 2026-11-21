//! Mercury core library. The GUI and terminal front ends are thin consumers of this crate.

pub mod agent;
pub mod approval;
pub mod checkpoints;
pub mod cli;
pub mod commands;
pub mod config;
pub mod discovery;
#[cfg(feature = "gui")]
pub mod gui;
pub mod hermes_import;
pub mod logo;
pub mod mcp;
pub mod memory;
pub mod nous;
pub mod profiles;
pub mod provider;
pub mod session;
pub mod skills;
pub mod tools;
