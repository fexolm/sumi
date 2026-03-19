use clap::Subcommand;

pub mod run;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the VM with the given program binary.
    Run(run::RunCommand),
}

impl Command {
    pub fn execute(self) -> Result<(), sumi_vm::error::Error> {
        match self {
            Self::Run(command) => command.execute(),
        }
    }
}
