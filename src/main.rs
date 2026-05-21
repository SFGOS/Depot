//! Depot - Not Your Average Package Manager
//! A source-based package manager for Linux

mod bootstrap;
mod build_options;
mod builder;
mod cli;
mod cli_assets;
mod commands;
mod config;
mod cross;
mod db;
mod deps;
mod fakeroot;
mod hex;
mod index;
mod install;
mod interrupts;
mod locking;
mod metadata_time;
mod package;
mod planner;
mod runtime_env;
mod shell_helpers;
mod signing;
mod source;
mod staging;
mod system_state;
#[cfg(test)]
mod test_support;
mod ui;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    commands::run(cli)
}
