mod doctor;
mod host;
mod policy;

use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(version, about = "CPU isolation for shared Linux servers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the single-host benchmark workloads and load generator.
    #[command(subcommand)]
    Bench(corereserve_bench::Command),
    /// Inspect live CPU telemetry and run the host control loop.
    #[command(subcommand)]
    Host(host::Command),
    /// Evaluate host CPU policy from recorded telemetry.
    #[command(subcommand)]
    Policy(policy::Command),
    /// Inspect host and cgroup capabilities without changing them.
    Doctor {
        /// Cgroup directory to inspect, typically the intended batch subtree.
        #[arg(long, default_value = "/sys/fs/cgroup")]
        cgroup: PathBuf,
        /// Use JSON to capture a versioned lab manifest.
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("corereserve: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Bench(command) => command.run()?,
        Command::Host(command) => command.run()?,
        Command::Policy(command) => command.run()?,
        Command::Doctor { cgroup, format } => {
            let manifest = doctor::Manifest::collect(&cgroup)?;
            let mut stdout = io::stdout().lock();
            match format {
                Format::Text => manifest.write_summary(&mut stdout)?,
                Format::Json => {
                    serde_json::to_writer_pretty(&mut stdout, &manifest)?;
                    writeln!(stdout)?;
                }
            }
        }
    }
    Ok(())
}
