use std::{
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, ValueEnum};
use corereserve_telemetry::{ServiceSample, ServiceWindow, boot_time_us};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinSet,
    time::{Instant, sleep_until, timeout_at},
};

use crate::{Result, histogram::Histogram, write_json};

#[derive(Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Scenario {
    ServiceOnly,
    Shared,
    StaticPartition,
}

#[derive(Args, Serialize)]
pub struct Settings {
    #[arg(long, default_value = "127.0.0.1:8400")]
    target: SocketAddr,
    /// Baseline label to store in the report.
    #[arg(long, value_enum)]
    scenario: Scenario,
    /// Scheduled requests per second.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..=100_000))]
    rate: u64,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    duration_secs: u64,
    /// Deadline measured from the scheduled arrival time.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=60_000))]
    timeout_ms: u64,
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..=65_536))]
    max_in_flight: u32,
    /// Optional CPU list for the load generator.
    #[arg(long)]
    pub(crate) cpus: Option<String>,
    /// Atomically publish the completed service window for the local host agent.
    #[arg(long, requires = "service_instance")]
    #[serde(skip)]
    telemetry_file: Option<PathBuf>,
    /// Enrolled service instance for the published window.
    #[arg(long, requires = "telemetry_file")]
    #[serde(skip)]
    service_instance: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Outcomes {
    successful: u64,
    timed_out: u64,
    transport_errors: u64,
    protocol_errors: u64,
    dropped_at_capacity: u64,
    expired_before_admission: u64,
}

pub struct TrialTelemetry {
    pub offered_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub client_missed_requests: u64,
    pub p999_us: Option<u64>,
}

/// Read the fields used by policy replay from a completed benchmark load report.
pub fn read_trial_telemetry(reader: impl Read) -> io::Result<TrialTelemetry> {
    #[derive(Deserialize)]
    struct Trial {
        schema_version: u32,
        offered_requests: u64,
        outcomes: Outcomes,
        successful_latency: Histogram,
    }

    let trial: Trial = serde_json::from_reader(reader)?;
    if trial.schema_version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported benchmark report schema",
        ));
    }
    let outcomes = trial.outcomes;
    let failed = u128::from(outcomes.timed_out)
        + u128::from(outcomes.transport_errors)
        + u128::from(outcomes.protocol_errors);
    let client_missed =
        u128::from(outcomes.dropped_at_capacity) + u128::from(outcomes.expired_before_admission);
    if failed + client_missed + u128::from(outcomes.successful)
        != u128::from(trial.offered_requests)
        || !trial
            .successful_latency
            .matches_sample_count(outcomes.successful)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark outcomes or histogram are inconsistent",
        ));
    }

    Ok(TrialTelemetry {
        offered_requests: trial.offered_requests,
        successful_requests: outcomes.successful,
        failed_requests: failed as u64,
        client_missed_requests: client_missed as u64,
        p999_us: trial.successful_latency.percentile(999),
    })
}

enum Outcome {
    Success(Duration),
    Timeout,
    TransportError,
    ProtocolError,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    settings: Settings,
    effective_cpus: Vec<usize>,
    offered_requests: u64,
    admitted_requests: u64,
    successful_requests_per_second: f64,
    unsuccessful_fraction: f64,
    wall_time_secs: f64,
    outcomes: Outcomes,
    p50_us: Option<u64>,
    p99_us: Option<u64>,
    p999_us: Option<u64>,
    successful_latency: Histogram,
    admission_lag: Histogram,
}

pub(crate) async fn run(settings: Settings, cpus: Vec<usize>) -> Result<()> {
    let offered = settings.rate * settings.duration_secs;
    let duration = Duration::from_secs(settings.duration_secs);
    let timeout = Duration::from_millis(settings.timeout_ms);
    let mut requests = JoinSet::new();
    let mut outcomes = Outcomes::default();
    let mut latency = Histogram::default();
    let mut admission_lag = Histogram::default();
    let mut admitted = 0;
    let start = Instant::now();

    for nonce in 0..offered {
        let scheduled = start + Duration::from_nanos(nonce * 1_000_000_000 / settings.rate);
        while Instant::now() < scheduled {
            tokio::select! {
                biased;
                Some(result) = requests.join_next() => record(result?, &mut outcomes, &mut latency),
                _ = sleep_until(scheduled) => break,
            }
        }
        while let Some(result) = requests.try_join_next() {
            record(result?, &mut outcomes, &mut latency);
        }

        let now = Instant::now();
        let deadline = scheduled + timeout;
        if now >= deadline {
            outcomes.expired_before_admission += 1;
        } else if requests.len() >= settings.max_in_flight as usize {
            outcomes.dropped_at_capacity += 1;
        } else {
            admitted += 1;
            admission_lag.record(now - scheduled);
            requests.spawn(request(settings.target, nonce, scheduled, deadline));
        }
        // Past-due arrivals can otherwise loop without polling any of the requests.
        if now >= scheduled + Duration::from_nanos(1_000_000_000 / settings.rate) {
            tokio::task::yield_now().await;
        }
    }
    while let Some(result) = requests.join_next().await {
        record(result?, &mut outcomes, &mut latency);
    }
    sleep_until(start + duration).await;

    let successful_requests_per_second = outcomes.successful as f64 / duration.as_secs_f64();
    let unsuccessful_fraction = (offered - outcomes.successful) as f64 / offered as f64;
    let p999_us = latency.percentile(999);
    let publish_result = if let (Some(path), Some(instance)) =
        (&settings.telemetry_file, &settings.service_instance)
    {
        let sample = ServiceSample {
            observed_at_ms: boot_time_us() / 1_000,
            service_instance: instance.clone(),
            offered_requests: offered,
            successful_requests: outcomes.successful,
            failed_requests: outcomes.timed_out
                + outcomes.transport_errors
                + outcomes.protocol_errors,
            client_missed_requests: outcomes.dropped_at_capacity
                + outcomes.expired_before_admission,
            p999_us,
        };
        publish_window(path, sample)
    } else {
        Ok(())
    };
    write_json(
        io::stdout().lock(),
        &Report {
            schema_version: 1,
            settings,
            effective_cpus: cpus,
            offered_requests: offered,
            admitted_requests: admitted,
            successful_requests_per_second,
            unsuccessful_fraction,
            wall_time_secs: start.elapsed().as_secs_f64(),
            outcomes,
            p50_us: latency.percentile(500),
            p99_us: latency.percentile(990),
            p999_us,
            successful_latency: latency,
            admission_lag,
        },
    )?;
    publish_result
}

fn publish_window(path: &Path, sample: ServiceSample) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    write_json(file.as_file_mut(), &ServiceWindow::new(sample)?)?;
    file.persist(path)?;
    Ok(())
}

async fn request(target: SocketAddr, nonce: u64, scheduled: Instant, deadline: Instant) -> Outcome {
    if Instant::now() >= deadline {
        return Outcome::Timeout;
    }
    let nonce = nonce.to_be_bytes();
    let result = timeout_at(deadline, async {
        let mut socket = TcpStream::connect(target).await?;
        socket.write_all(&nonce).await?;
        let mut response = [0; 8];
        socket.read_exact(&mut response).await?;
        Ok::<_, io::Error>(response)
    })
    .await;
    let now = Instant::now();
    if now >= deadline {
        return Outcome::Timeout;
    }
    match result {
        Ok(Ok(response)) if response == nonce => Outcome::Success(now - scheduled),
        Ok(Ok(_)) => Outcome::ProtocolError,
        Ok(Err(_)) => Outcome::TransportError,
        Err(_) => Outcome::Timeout,
    }
}

fn record(outcome: Outcome, outcomes: &mut Outcomes, latency: &mut Histogram) {
    match outcome {
        Outcome::Success(elapsed) => {
            outcomes.successful += 1;
            latency.record(elapsed);
        }
        Outcome::Timeout => outcomes.timed_out += 1,
        Outcome::TransportError => outcomes.transport_errors += 1,
        Outcome::ProtocolError => outcomes.protocol_errors += 1,
    }
}
