use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, Subcommand};
use corereserve_agent::{
    CpuPressureSample, Decision, Policy, PolicyConfig, State,
    cgroup::{Actuator, Cgroup, CpuMax, CpuStat},
};
use corereserve_telemetry::{ClockIdentity, ServiceWindow, boot_time_us};
use serde::Serialize;
use tokio::{
    runtime::Builder,
    signal::unix::{SignalKind, signal},
    time::{Instant, sleep_until},
};

use crate::policy::read_json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Read CPU quota, pressure, and accounting for the service and batch cgroups.
    Status {
        #[command(flatten)]
        paths: CgroupPaths,
    },
    /// Run the live policy loop. Add --apply to change the batch quota.
    Run {
        #[command(flatten)]
        paths: CgroupPaths,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        telemetry_file: PathBuf,
        #[arg(long)]
        apply: bool,
        #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(10..=60_000))]
        sample_interval_ms: u64,
        /// Exit gracefully after this many evaluations.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        ticks: Option<u64>,
    },
    /// Apply the conservative ceiling once, for example from systemd ExecStopPost.
    Cap {
        #[command(flatten)]
        paths: CgroupPaths,
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Args)]
pub(crate) struct CgroupPaths {
    #[arg(long)]
    service_cgroup: PathBuf,
    #[arg(long)]
    batch_cgroup: PathBuf,
}

impl CgroupPaths {
    fn open(self) -> io::Result<(Cgroup, Cgroup)> {
        Ok((
            Cgroup::open(&self.service_cgroup)?,
            Cgroup::open(&self.batch_cgroup)?,
        ))
    }
}

impl Command {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Status { paths } => {
                let (service, batch) = paths.open()?;
                print_json(&serde_json::json!({
                    "schema_version": 1,
                    "service": service.status()?,
                    "batch": batch.status()?,
                }))
            }
            Self::Cap { paths, config } => {
                let config: PolicyConfig = read_json(&config)?;
                let ceiling = CpuMax::limited(config.unknown_quota_us, config.cpu_period_us);
                Policy::new(config)?;
                let (service, batch) = paths.open()?;
                let actual = batch.lock_batch(&service)?.reconcile(ceiling, false)?;
                print_json(
                    &serde_json::json!({ "schema_version": 1, "event": "capped", "actual_cpu_max": actual }),
                )
            }
            Self::Run {
                paths,
                config,
                telemetry_file,
                apply,
                sample_interval_ms,
                ticks,
            } => {
                let config = read_json(&config)?;
                let (service, batch) = paths.open()?;
                Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(async {
                        let mut interrupt = signal(SignalKind::interrupt())?;
                        let mut terminate = signal(SignalKind::terminate())?;
                        let mut host = Host::new(config, &service, &batch, apply)?;
                        let sample_interval = Duration::from_millis(sample_interval_ms);
                        let mut next_tick = Instant::now();
                        let mut remaining = ticks;
                        let loop_result = loop {
                            tokio::select! {
                                biased;
                                _ = interrupt.recv() => break Ok(()),
                                _ = terminate.recv() => break Ok(()),
                                _ = sleep_until(next_tick) => {
                                    if let Err(error) = host.tick(&telemetry_file) {
                                        break Err(error);
                                    }
                                    if let Some(left) = remaining.as_mut() {
                                        *left -= 1;
                                        if *left == 0 {
                                            break Ok(());
                                        }
                                    }
                                    next_tick = Instant::now() + sample_interval;
                                }
                            }
                        };
                        let shutdown_result = host.shutdown();
                        if let Err(error) = loop_result {
                            if let Err(shutdown_error) = shutdown_result {
                                eprintln!(
                                    "corereserve: shutdown quota cap failed: {shutdown_error}"
                                );
                            }
                            return Err(error);
                        }
                        shutdown_result
                    })
            }
        }
    }
}

struct Host<'a> {
    service: &'a Cgroup,
    batch: &'a Cgroup,
    actuator: Option<Actuator<'a>>,
    ceiling: CpuMax,
    confirmed: Option<CpuMax>,
    policy: Policy,
    clock: ClockIdentity,
    service_instance: String,
    latest_service_ms: Option<u64>,
    pressure: PressureTracker,
}

#[derive(Serialize)]
struct TickAudit<'a> {
    schema_version: u32,
    mode: &'static str,
    #[serde(flatten)]
    decision: Decision<'a>,
    actual_cpu_max: Option<CpuMax>,
    quota_rebased: bool,
    quota_verified: bool,
    effective_admit_batch: bool,
    service_cpu_stat: Option<CpuStat>,
    batch_cpu_stat: Option<CpuStat>,
    errors: Vec<String>,
}

impl<'a> Host<'a> {
    fn new(
        config: PolicyConfig,
        service: &'a Cgroup,
        batch: &'a Cgroup,
        apply: bool,
    ) -> Result<Self> {
        let ceiling = CpuMax::limited(config.unknown_quota_us, config.cpu_period_us);
        let pressure = PressureTracker {
            previous: None,
            max_age_ms: config.cpu_pressure_max_age_ms,
        };
        let service_instance = config.service_instance.clone();
        let mut policy = Policy::new(config)?;
        let clock = ClockIdentity::read()?;
        let actuator = apply.then(|| batch.lock_batch(service)).transpose()?;
        let confirmed = actuator
            .as_ref()
            .map(|actuator| actuator.reconcile(ceiling, false))
            .transpose()?;
        if let Some(actual) = confirmed {
            policy.reset_from_quota(actual.quota_in_period(ceiling.period_us));
        }
        Ok(Self {
            service,
            batch,
            actuator,
            ceiling,
            confirmed,
            policy,
            clock,
            service_instance,
            latest_service_ms: None,
            pressure,
        })
    }

    fn tick(&mut self, telemetry_file: &Path) -> Result<()> {
        let mut errors = Vec::new();
        let mut service = evidence(
            ServiceWindow::read(telemetry_file, &self.clock),
            "service window",
            &mut errors,
        );
        let total = evidence(
            self.service.pressure_some_total(),
            "service CPU pressure",
            &mut errors,
        );
        let now = boot_time_us();
        if let Some(sample) = &service
            && sample.service_instance == self.service_instance
            && sample.observed_at_ms <= now / 1_000
        {
            if self
                .latest_service_ms
                .is_some_and(|latest| sample.observed_at_ms < latest)
            {
                errors.push("service window timestamp regressed".to_owned());
                service = None;
            } else {
                self.latest_service_ms = Some(sample.observed_at_ms);
            }
        }
        let pressure = self.pressure.observe(now, total);
        let mut actual = evidence(self.batch.cpu_max(), "batch cpu.max", &mut errors);
        let mut verified = false;
        let mut rebased = false;
        if let Some(actuator) = &self.actuator
            && (actual.is_none() || self.confirmed != actual)
        {
            rebased = true;
            actual = evidence(
                actuator.reconcile(self.ceiling, false),
                "reconcile conservative ceiling",
                &mut errors,
            );
            self.confirmed = actual;
            self.policy.reset_from_quota(
                actual.and_then(|quota| quota.quota_in_period(self.ceiling.period_us)),
            );
        }
        let can_evaluate = self.actuator.is_none() || self.confirmed.is_some();
        let decision = self.policy.evaluate(
            now / 1_000,
            service.as_ref().filter(|_| can_evaluate),
            pressure.as_ref(),
        )?;
        if let (Some(actuator), Some(expected)) = (&self.actuator, self.confirmed) {
            let desired = CpuMax::limited(decision.proposed_quota_us, decision.cpu_period_us);
            let can_increase = matches!(decision.state, State::Healthy | State::Recovering)
                && decision.proposed_quota_us > decision.previous_quota_us;
            match actuator.reconcile_from(expected, desired, can_increase) {
                Ok(quota) => {
                    actual = Some(quota);
                    self.confirmed = actual;
                    verified = true;
                }
                Err(error) => {
                    errors.push(format!("apply policy quota: {error}"));
                    let fallback = CpuMax::limited(
                        decision
                            .proposed_quota_us
                            .min(self.ceiling.quota_us.expect("finite ceiling")),
                        self.ceiling.period_us,
                    );
                    self.confirmed = evidence(
                        actuator.reconcile(fallback, false),
                        "apply fallback ceiling",
                        &mut errors,
                    );
                    actual = self.confirmed.or_else(|| self.batch.cpu_max().ok());
                    self.policy.reset_from_quota(
                        self.confirmed
                            .and_then(|quota| quota.quota_in_period(self.ceiling.period_us)),
                    );
                }
            }
        }
        let service_cpu_stat = evidence(
            self.service.cpu_stat(),
            "service CPU accounting",
            &mut errors,
        );
        let batch_cpu_stat = evidence(self.batch.cpu_stat(), "batch CPU accounting", &mut errors);
        print_json(&TickAudit {
            schema_version: 1,
            mode: if self.actuator.is_some() {
                "apply"
            } else {
                "observe"
            },
            actual_cpu_max: actual,
            quota_rebased: rebased,
            quota_verified: verified,
            effective_admit_batch: verified && decision.admit_batch,
            service_cpu_stat,
            batch_cpu_stat,
            decision,
            errors,
        })
    }

    fn shutdown(&self) -> Result<()> {
        if let Some(actuator) = &self.actuator {
            let actual = actuator.reconcile(self.ceiling, false)?;
            print_json(
                &serde_json::json!({ "schema_version": 1, "event": "shutdown_capped", "actual_cpu_max": actual }),
            )?;
        }
        Ok(())
    }
}

struct PressureTracker {
    previous: Option<(u64, u64)>,
    max_age_ms: u64,
}

impl PressureTracker {
    fn observe(&mut self, now_us: u64, total_us: Option<u64>) -> Option<CpuPressureSample> {
        let previous = std::mem::replace(&mut self.previous, total_us.map(|total| (now_us, total)));
        let (previous_time, previous_total) = previous?;
        let elapsed = now_us.checked_sub(previous_time)?;
        if elapsed == 0 || elapsed > self.max_age_ms.saturating_mul(1_000) {
            return None;
        }
        let stall = total_us?.checked_sub(previous_total)?;
        let some_ppm = (u128::from(stall) * 1_000_000)
            .div_ceil(u128::from(elapsed))
            .min(1_000_000) as u32;
        Some(CpuPressureSample {
            observed_at_ms: now_us / 1_000,
            some_ppm,
        })
    }
}

fn evidence<T>(result: io::Result<T>, source: &str, errors: &mut Vec<String>) -> Option<T> {
    result
        .map_err(|error| errors.push(format!("{source}: {error}")))
        .ok()
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_tracker_discards_gaps_and_counter_resets() {
        let mut tracker = PressureTracker {
            previous: None,
            max_age_ms: 250,
        };
        assert!(tracker.observe(1_000, Some(1)).is_none());
        assert_eq!(tracker.observe(2_000, Some(251)).unwrap().some_ppm, 250_000);
        assert!(tracker.observe(3_000, Some(10)).is_none());
        assert!(tracker.observe(254_000, Some(11)).is_none());
        assert!(tracker.observe(255_000, None).is_none());
        assert!(tracker.observe(256_000, Some(12)).is_none());
    }
}
