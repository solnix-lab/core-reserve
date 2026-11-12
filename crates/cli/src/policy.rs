use std::{
    fs::File,
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
};

use clap::Subcommand;
use corereserve_agent::{CpuPressureSample, Decision, Policy, ServiceSample};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Replay telemetry and print proposed quota and admission decisions as JSON.
    DryRun {
        /// Versioned policy configuration file.
        #[arg(long)]
        config: PathBuf,
        /// Ordered timeline of service and CPU pressure observations.
        #[arg(long)]
        trace: PathBuf,
    },
}

impl Command {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::DryRun { config, trace } => dry_run(&config, &trace),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Trace {
    schema_version: u32,
    ticks: Vec<Tick>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tick {
    at_ms: u64,
    service: Option<ServiceObservation>,
    cpu_pressure: Option<CpuPressureSample>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceObservation {
    observed_at_ms: u64,
    service_instance: String,
    benchmark_report: PathBuf,
}

struct RecordedService {
    sample: ServiceSample,
    report: PathBuf,
}

#[derive(Serialize)]
struct AuditEntry<'a> {
    #[serde(flatten)]
    decision: Decision<'a>,
    benchmark_report: Option<&'a Path>,
}

#[derive(Serialize)]
struct Audit {
    schema_version: u32,
    decisions: Vec<serde_json::Value>,
}

fn dry_run(config_path: &Path, trace_path: &Path) -> Result<()> {
    let mut policy = Policy::new(read_json(config_path)?)?;
    let trace: Trace = read_json(trace_path)?;
    if trace.schema_version != 1 || trace.ticks.is_empty() {
        return Err("trace requires schema version 1 and at least one tick".into());
    }
    let trace_dir = trace_path.parent().unwrap_or_else(|| Path::new("."));
    let mut service: Option<RecordedService> = None;
    let mut pressure: Option<CpuPressureSample> = None;
    let mut decisions = Vec::with_capacity(trace.ticks.len());

    for tick in trace.ticks {
        if let Some(observation) = tick.service {
            check_observed_time(
                "service",
                tick.at_ms,
                observation.observed_at_ms,
                service
                    .as_ref()
                    .map(|previous| previous.sample.observed_at_ms),
            )?;
            service = Some(observation.read_report(trace_dir)?);
        }
        if let Some(observation) = tick.cpu_pressure {
            check_observed_time(
                "CPU pressure",
                tick.at_ms,
                observation.observed_at_ms,
                pressure.as_ref().map(|previous| previous.observed_at_ms),
            )?;
            pressure = Some(observation);
        }
        let decision = policy.evaluate(
            tick.at_ms,
            service.as_ref().map(|service| &service.sample),
            pressure.as_ref(),
        )?;
        decisions.push(serde_json::to_value(AuditEntry {
            decision,
            benchmark_report: service.as_ref().map(|service| service.report.as_path()),
        })?);
    }

    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(
        &mut stdout,
        &Audit {
            schema_version: 1,
            decisions,
        },
    )?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

impl ServiceObservation {
    fn read_report(self, trace_dir: &Path) -> Result<RecordedService> {
        let path = trace_dir.join(&self.benchmark_report);
        let trial = read_file(&path, corereserve_bench::read_trial_telemetry)?;
        Ok(RecordedService {
            sample: ServiceSample {
                observed_at_ms: self.observed_at_ms,
                service_instance: self.service_instance,
                offered_requests: trial.offered_requests,
                successful_requests: trial.successful_requests,
                failed_requests: trial.failed_requests,
                client_missed_requests: trial.client_missed_requests,
                p999_us: trial.p999_us,
            },
            report: self.benchmark_report,
        })
    }
}

fn check_observed_time(
    source: &str,
    tick: u64,
    observed: u64,
    previous: Option<u64>,
) -> Result<()> {
    if observed > tick || previous.is_some_and(|previous| observed <= previous) {
        return Err(format!(
            "{source} observation at {observed} ms must follow the previous observation and cannot follow its tick at {tick} ms"
        ).into());
    }
    Ok(())
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    read_file(path, serde_json::from_reader)
}

fn read_file<T, E: std::fmt::Display>(
    path: &Path,
    read: impl FnOnce(BufReader<File>) -> std::result::Result<T, E>,
) -> Result<T> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    read(BufReader::new(file)).map_err(|error| format!("{}: {error}", path.display()).into())
}
