mod cli;
mod doctor;
mod manifest;
mod metal;
mod packages;
mod paths;
mod profile;
mod util;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::paths::AppPaths;

fn main() {
    if let Err(error) = run() {
        eprintln!("dotlab: error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;

    match cli.command {
        Command::Doctor(args) => doctor::run(&paths, args),
        Command::Init(args) => profile::init(&paths, args),
        Command::Profile(command) => profile::run(&paths, command),
        Command::Switch(args) => profile::switch(&paths, args),
        Command::Rollback(args) => profile::rollback(&paths, args),
        Command::Packages(command) => packages::run(&paths, command),
        Command::Metal(command) => metal::run(&paths, command),
        Command::Version => {
            println!("dotlab {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
