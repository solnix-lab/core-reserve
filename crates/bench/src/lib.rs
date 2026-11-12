mod batch;
mod cpu;
mod histogram;
mod load;
mod service;

pub use load::{TrialTelemetry, read_trial_telemetry};

use std::io::{self, Write};

use clap::Subcommand;
use serde::Serialize;
use tokio::{
    runtime::Builder,
    signal::unix::{Signal, SignalKind, signal},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub enum Command {
    /// Run a synthetic CPU-bound TCP service.
    Serve(service::Settings),
    /// Send open-loop traffic and print a JSON trial report.
    Load(load::Settings),
    /// Run a CPU batch workload and print a JSON work report.
    Batch(batch::Settings),
}

impl Command {
    pub fn run(self) -> Result<()> {
        let requested_cpus = match &self {
            Self::Serve(settings) => settings.cpus.as_deref(),
            Self::Load(settings) => settings.cpus.as_deref(),
            Self::Batch(settings) => settings.cpus.as_deref(),
        };
        // Set affinity before the runtime or batch workers create any threads.
        let cpus = cpu::apply_affinity(requested_cpus)?;
        Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async move {
                match self {
                    Self::Serve(settings) => service::run(settings, cpus).await,
                    Self::Load(settings) => load::run(settings, cpus).await,
                    Self::Batch(settings) => batch::run(settings, cpus).await,
                }
            })
    }
}

fn write_json(mut out: impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn cpu_work(seed: u64, iterations: u32) {
    let mut value = std::hint::black_box(seed);
    for _ in 0..iterations {
        value ^= value.rotate_left(13);
        value = value.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        value ^= value >> 17;
    }
    std::hint::black_box(value);
}

struct Shutdown {
    interrupt: Signal,
    terminate: Signal,
}

impl Shutdown {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn wait(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}
