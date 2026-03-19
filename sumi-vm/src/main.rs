mod cmd;

use clap::Parser;
use std::process;

#[derive(Debug, Parser)]
#[command(name = "sumi-vm", arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: cmd::Command,
}

pub fn main() {
    if let Err(err) = Cli::parse().command.execute() {
        eprintln!("{err}");
        process::exit(1);
    }
}
