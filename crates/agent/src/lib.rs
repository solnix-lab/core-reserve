pub mod cgroup;

pub use corereserve_telemetry::ServiceSample;
use serde::{Deserialize, Serialize};

const PPM: u32 = 1_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub schema_version: u32,
    pub revision: u64,
    pub service_instance: String,
    pub cpu_period_us: u64,
    pub min_quota_us: u64,
    pub unknown_quota_us: u64,
    pub max_quota_us: u64,
    pub service_max_age_ms: u64,
    pub cpu_pressure_max_age_ms: u64,
    pub min_offered_requests: u64,
    pub min_successful_requests: u64,
    pub max_p999_us: u64,
    pub max_error_ppm: u32,
    pub cpu_pressure_enter_ppm: u32,
    pub cpu_pressure_exit_ppm: u32,
    pub recovery_window_ms: u64,
    pub recovery_step_interval_ms: u64,
    pub recovery_step_quota_us: u64,
}

impl PolicyConfig {
    fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported policy config schema");
        }
        if self.revision == 0 || self.service_instance.trim().is_empty() {
            return Err("policy needs a positive revision and a service instance");
        }
        if !(1_000..=1_000_000).contains(&self.cpu_period_us)
            || self.min_quota_us < 1_000
            || self.min_quota_us > self.unknown_quota_us
            || self.unknown_quota_us > self.max_quota_us
        {
            return Err("invalid CPU period or quota bounds");
        }
        if self.cpu_pressure_exit_ppm >= self.cpu_pressure_enter_ppm
            || self.cpu_pressure_enter_ppm > PPM
            || self.max_error_ppm > PPM
        {
            return Err("invalid CPU pressure or error thresholds");
        }
        if [
            self.service_max_age_ms,
            self.cpu_pressure_max_age_ms,
            self.min_offered_requests,
            self.min_successful_requests,
            self.max_p999_us,
            self.recovery_window_ms,
            self.recovery_step_interval_ms,
            self.recovery_step_quota_us,
        ]
        .contains(&0)
        {
            return Err("sample limits, latency limit, and recovery parameters must be positive");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuPressureSample {
    pub observed_at_ms: u64,
    pub some_ppm: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Healthy,
    Constrained,
    Recovering,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    EvidenceExpiredBetweenTicks,
    ServiceMissing,
    ServiceStale,
    ServiceInstanceMismatch,
    ServiceSampleInvalid,
    ClientMissedRequests,
    InsufficientOfferedRequests,
    InsufficientLatencySamples,
    ErrorBudgetExceeded,
    LatencyLimitExceeded,
    CpuPressureMissing,
    CpuPressureStale,
    CpuPressureInvalid,
    CpuPressureHigh,
    CpuPressureAboveRecoveryThreshold,
    WaitingForStableWindow,
    WaitingForRecoveryStep,
    QuotaIncreased,
    WithinLimits,
}

#[derive(Serialize)]
pub struct Decision<'a> {
    pub at_ms: u64,
    pub policy_revision: u64,
    pub state: State,
    pub previous_quota_us: u64,
    pub proposed_quota_us: u64,
    pub cpu_period_us: u64,
    pub admit_batch: bool,
    pub reasons: Vec<Reason>,
    pub service_error_ppm: Option<u32>,
    pub service: Option<&'a ServiceSample>,
    pub cpu_pressure: Option<&'a CpuPressureSample>,
}

/// Deterministic host policy. The caller supplies monotonic timestamps and the latest observations.
pub struct Policy {
    config: PolicyConfig,
    state: State,
    quota_us: u64,
    last_tick_ms: Option<u64>,
    evidence_valid_through_ms: Option<u64>,
    stable_since_ms: Option<u64>,
    last_increase_ms: Option<u64>,
}

impl Policy {
    pub fn new(config: PolicyConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self {
            state: State::Unknown,
            quota_us: config.unknown_quota_us,
            config,
            last_tick_ms: None,
            evidence_valid_through_ms: None,
            stable_since_ms: None,
            last_increase_ms: None,
        })
    }

    /// Begin conservative recovery from a verified existing quota expressed in the policy period.
    pub fn reset_from_quota(&mut self, existing_quota_us: Option<u64>) {
        self.state = State::Unknown;
        self.quota_us = existing_quota_us
            .unwrap_or(self.config.unknown_quota_us)
            .min(self.config.unknown_quota_us);
        self.evidence_valid_through_ms = None;
        self.reset_recovery();
    }

    pub fn evaluate<'a>(
        &mut self,
        at_ms: u64,
        service: Option<&'a ServiceSample>,
        cpu_pressure: Option<&'a CpuPressureSample>,
    ) -> Result<Decision<'a>, &'static str> {
        if self.last_tick_ms.is_some_and(|last| at_ms <= last) {
            return Err("policy tick times must be strictly increasing");
        }
        self.last_tick_ms = Some(at_ms);
        let previous_quota_us = self.quota_us;
        let mut reasons = Vec::new();

        // A fresh replacement at this tick cannot establish what happened during a gap.
        if self
            .evidence_valid_through_ms
            .is_some_and(|valid_through| at_ms > valid_through)
        {
            self.state = State::Unknown;
            self.quota_us = self.quota_us.min(self.config.unknown_quota_us);
            self.reset_recovery();
            reasons.push(Reason::EvidenceExpiredBetweenTicks);
        }

        let service = assess_service(&self.config, at_ms, service, &mut reasons);
        let pressure = assess_pressure(&self.config, at_ms, cpu_pressure, &mut reasons);
        self.evidence_valid_through_ms = service
            .valid_through_ms
            .zip(pressure.map(|(_, through)| through))
            .map(|(service, pressure)| service.min(pressure));

        let pressure_high =
            pressure.is_some_and(|(ppm, _)| ppm >= self.config.cpu_pressure_enter_ppm);
        if pressure_high {
            reasons.push(Reason::CpuPressureHigh);
        }
        let breach = pressure_high || service.breach;
        if self.evidence_valid_through_ms.is_none() {
            self.state = State::Unknown;
            self.quota_us = self.quota_us.min(self.config.unknown_quota_us);
            self.reset_recovery();
        } else if breach {
            self.state = State::Constrained;
            self.reset_recovery();
        } else if self.state == State::Healthy {
            reasons.push(Reason::WithinLimits);
        } else if pressure.is_some_and(|(ppm, _)| ppm > self.config.cpu_pressure_exit_ppm) {
            self.state = State::Constrained;
            self.reset_recovery();
            reasons.push(Reason::CpuPressureAboveRecoveryThreshold);
        } else {
            self.recover(at_ms, &mut reasons);
        }
        if breach {
            self.quota_us = self.quota_us.min(self.config.min_quota_us);
        }

        Ok(Decision {
            at_ms,
            policy_revision: self.config.revision,
            state: self.state,
            previous_quota_us,
            proposed_quota_us: self.quota_us,
            cpu_period_us: self.config.cpu_period_us,
            admit_batch: self.state == State::Healthy,
            reasons,
            service_error_ppm: service.error_ppm,
            service: service.sample,
            cpu_pressure,
        })
    }

    fn recover(&mut self, at_ms: u64, reasons: &mut Vec<Reason>) {
        self.state = State::Recovering;
        let stable_since = *self.stable_since_ms.get_or_insert(at_ms);
        if at_ms - stable_since < self.config.recovery_window_ms {
            reasons.push(Reason::WaitingForStableWindow);
            return;
        }
        if self.quota_us < self.config.max_quota_us {
            let step_due = self
                .last_increase_ms
                .is_none_or(|last| at_ms - last >= self.config.recovery_step_interval_ms);
            if !step_due {
                reasons.push(Reason::WaitingForRecoveryStep);
                return;
            }
            self.quota_us = self
                .quota_us
                .saturating_add(self.config.recovery_step_quota_us)
                .min(self.config.max_quota_us);
            self.last_increase_ms = Some(at_ms);
            reasons.push(Reason::QuotaIncreased);
        }
        if self.quota_us == self.config.max_quota_us {
            self.state = State::Healthy;
            reasons.push(Reason::WithinLimits);
        }
    }

    fn reset_recovery(&mut self) {
        self.stable_since_ms = None;
        self.last_increase_ms = None;
    }
}

#[derive(Default)]
struct ServiceAssessment<'a> {
    sample: Option<&'a ServiceSample>,
    valid_through_ms: Option<u64>,
    error_ppm: Option<u32>,
    breach: bool,
}

fn assess_service<'a>(
    config: &PolicyConfig,
    at_ms: u64,
    service: Option<&'a ServiceSample>,
    reasons: &mut Vec<Reason>,
) -> ServiceAssessment<'a> {
    let mut assessment = ServiceAssessment {
        sample: service,
        ..ServiceAssessment::default()
    };
    let Some(sample) = service else {
        reasons.push(Reason::ServiceMissing);
        return assessment;
    };
    let valid_through = sample
        .observed_at_ms
        .saturating_add(config.service_max_age_ms);
    let accounted = u128::from(sample.successful_requests)
        + u128::from(sample.failed_requests)
        + u128::from(sample.client_missed_requests);
    let unavailable = if sample.service_instance != config.service_instance {
        Some(Reason::ServiceInstanceMismatch)
    } else if sample.observed_at_ms > at_ms
        || accounted != u128::from(sample.offered_requests)
        || sample.p999_us.is_some() != (sample.successful_requests > 0)
    {
        Some(Reason::ServiceSampleInvalid)
    } else if at_ms > valid_through {
        Some(Reason::ServiceStale)
    } else if sample.client_missed_requests > 0 {
        Some(Reason::ClientMissedRequests)
    } else if sample.offered_requests < config.min_offered_requests {
        Some(Reason::InsufficientOfferedRequests)
    } else {
        None
    };
    if let Some(reason) = unavailable {
        reasons.push(reason);
        return assessment;
    }

    let error_ppm = (u128::from(sample.failed_requests) * u128::from(PPM))
        .div_ceil(u128::from(sample.offered_requests)) as u32;
    assessment.error_ppm = Some(error_ppm);
    if error_ppm > config.max_error_ppm {
        assessment.breach = true;
        reasons.push(Reason::ErrorBudgetExceeded);
    }
    let enough_latency_samples = sample.successful_requests >= config.min_successful_requests;
    if !enough_latency_samples {
        reasons.push(Reason::InsufficientLatencySamples);
    } else if sample
        .p999_us
        .is_some_and(|latency| latency > config.max_p999_us)
    {
        assessment.breach = true;
        reasons.push(Reason::LatencyLimitExceeded);
    }
    // A clear error-budget violation is actionable even with too few successful requests for p99.9.
    if enough_latency_samples || assessment.breach {
        assessment.valid_through_ms = Some(valid_through);
    }
    assessment
}

fn assess_pressure(
    config: &PolicyConfig,
    at_ms: u64,
    pressure: Option<&CpuPressureSample>,
    reasons: &mut Vec<Reason>,
) -> Option<(u32, u64)> {
    let Some(pressure) = pressure else {
        reasons.push(Reason::CpuPressureMissing);
        return None;
    };
    let valid_through = pressure
        .observed_at_ms
        .saturating_add(config.cpu_pressure_max_age_ms);
    if pressure.observed_at_ms > at_ms || pressure.some_ppm > PPM {
        reasons.push(Reason::CpuPressureInvalid);
        None
    } else if at_ms > valid_through {
        reasons.push(Reason::CpuPressureStale);
        None
    } else {
        Some((pressure.some_ppm, valid_through))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PolicyConfig {
        PolicyConfig {
            schema_version: 1,
            revision: 1,
            service_instance: "rpc".to_owned(),
            cpu_period_us: 100_000,
            min_quota_us: 25_000,
            unknown_quota_us: 50_000,
            max_quota_us: 100_000,
            service_max_age_ms: 1_000,
            cpu_pressure_max_age_ms: 1_000,
            min_offered_requests: 1_000,
            min_successful_requests: 1_000,
            max_p999_us: 5_000,
            max_error_ppm: 1_000,
            cpu_pressure_enter_ppm: 150_000,
            cpu_pressure_exit_ppm: 50_000,
            recovery_window_ms: 100,
            recovery_step_interval_ms: 50,
            recovery_step_quota_us: 25_000,
        }
    }

    fn good_service(observed_at_ms: u64) -> ServiceSample {
        ServiceSample {
            observed_at_ms,
            service_instance: "rpc".to_owned(),
            offered_requests: 2_000,
            successful_requests: 2_000,
            failed_requests: 0,
            client_missed_requests: 0,
            p999_us: Some(3_000),
        }
    }

    #[test]
    fn holds_through_hysteresis_and_never_catches_up_missed_steps() {
        let mut policy = Policy::new(config()).unwrap();
        let service = good_service(0);
        for (time, pressure_ppm, state, quota) in [
            (0, 40_000, State::Recovering, 50_000),
            (100, 40_000, State::Recovering, 75_000),
            (149, 40_000, State::Recovering, 75_000),
            (150, 40_000, State::Healthy, 100_000),
            (151, 100_000, State::Healthy, 100_000),
            (152, 150_000, State::Constrained, 25_000),
            (153, 40_000, State::Recovering, 25_000),
            (240, 100_000, State::Constrained, 25_000),
            (241, 50_000, State::Recovering, 25_000),
            (600, 40_000, State::Recovering, 50_000),
            (650, 40_000, State::Recovering, 75_000),
            (700, 40_000, State::Healthy, 100_000),
        ] {
            let pressure = CpuPressureSample {
                observed_at_ms: time,
                some_ppm: pressure_ppm,
            };
            let decision = policy
                .evaluate(time, Some(&service), Some(&pressure))
                .unwrap();
            assert_eq!(decision.state, state, "tick {time}");
            assert_eq!(decision.proposed_quota_us, quota, "tick {time}");
            assert_eq!(decision.admit_batch, state == State::Healthy);
        }
    }

    #[test]
    fn error_budget_can_cut_quota_when_latency_sample_is_too_small() {
        let mut policy = Policy::new(config()).unwrap();
        let service = ServiceSample {
            successful_requests: 150,
            failed_requests: 1_850,
            p999_us: Some(99_000),
            ..good_service(0)
        };
        let pressure = CpuPressureSample {
            observed_at_ms: 0,
            some_ppm: 0,
        };
        let decision = policy.evaluate(0, Some(&service), Some(&pressure)).unwrap();
        assert_eq!(decision.state, State::Constrained);
        assert_eq!(decision.proposed_quota_us, 25_000);
        assert_eq!(decision.service_error_ppm, Some(925_000));
        assert!(decision.reasons.contains(&Reason::ErrorBudgetExceeded));
        assert!(
            decision
                .reasons
                .contains(&Reason::InsufficientLatencySamples)
        );
        assert!(!decision.reasons.contains(&Reason::LatencyLimitExceeded));

        let stale = policy
            .evaluate(1_001, Some(&service), Some(&pressure))
            .unwrap();
        assert_eq!(stale.state, State::Unknown);
        assert!(!stale.admit_batch);
        assert_eq!(stale.proposed_quota_us, 25_000);
        assert!(stale.reasons.contains(&Reason::ServiceStale));
        assert!(stale.reasons.contains(&Reason::CpuPressureStale));
    }

    #[test]
    fn fresh_samples_after_a_gap_restart_recovery_at_the_conservative_ceiling() {
        let mut policy = Policy::new(config()).unwrap();
        let service = good_service(0);
        let pressure = CpuPressureSample {
            observed_at_ms: 0,
            some_ppm: 0,
        };
        for time in [0, 100, 150] {
            policy
                .evaluate(time, Some(&service), Some(&pressure))
                .unwrap();
        }
        let fresh_service = good_service(1_001);
        let fresh_pressure = CpuPressureSample {
            observed_at_ms: 1_001,
            some_ppm: 0,
        };
        let decision = policy
            .evaluate(1_001, Some(&fresh_service), Some(&fresh_pressure))
            .unwrap();
        assert_eq!(decision.previous_quota_us, 100_000);
        assert_eq!(decision.proposed_quota_us, 50_000);
        assert_eq!(decision.state, State::Recovering);
        assert!(
            decision
                .reasons
                .contains(&Reason::EvidenceExpiredBetweenTicks)
        );
        assert!(
            policy
                .evaluate(1_001, Some(&fresh_service), Some(&fresh_pressure))
                .is_err()
        );
    }

    #[test]
    fn client_overload_and_wrong_service_block_admission() {
        let mut policy = Policy::new(config()).unwrap();
        let mut service = good_service(0);
        let pressure = CpuPressureSample {
            observed_at_ms: 0,
            some_ppm: 0,
        };
        service.successful_requests -= 1;
        service.client_missed_requests = 1;
        let overloaded = policy.evaluate(0, Some(&service), Some(&pressure)).unwrap();
        assert_eq!(overloaded.state, State::Unknown);
        assert_eq!(overloaded.service_error_ppm, None);
        assert!(overloaded.reasons.contains(&Reason::ClientMissedRequests));

        service.service_instance = "other".to_owned();
        let wrong_service = policy.evaluate(1, Some(&service), Some(&pressure)).unwrap();
        assert_eq!(wrong_service.state, State::Unknown);
        assert!(
            wrong_service
                .reasons
                .contains(&Reason::ServiceInstanceMismatch)
        );
    }

    #[test]
    fn fresh_pressure_or_latency_can_independently_cut_quota() {
        let mut policy = Policy::new(config()).unwrap();
        let pressure = CpuPressureSample {
            observed_at_ms: 0,
            some_ppm: 200_000,
        };
        let decision = policy.evaluate(0, None, Some(&pressure)).unwrap();
        assert_eq!(decision.state, State::Unknown);
        assert_eq!(decision.proposed_quota_us, 25_000);
        assert!(decision.reasons.contains(&Reason::CpuPressureHigh));

        let service = ServiceSample {
            p999_us: Some(5_001),
            ..good_service(1)
        };
        let pressure = CpuPressureSample {
            observed_at_ms: 1,
            some_ppm: 0,
        };
        let decision = policy.evaluate(1, Some(&service), Some(&pressure)).unwrap();
        assert_eq!(decision.state, State::Constrained);
        assert!(decision.reasons.contains(&Reason::LatencyLimitExceeded));
    }

    #[test]
    fn unknown_and_constrained_keep_a_stricter_existing_quota() {
        let mut policy = Policy::new(config()).unwrap();
        policy.reset_from_quota(Some(2_000));
        let unknown = policy.evaluate(0, None, None).unwrap();
        assert_eq!(unknown.state, State::Unknown);
        assert_eq!(unknown.proposed_quota_us, 2_000);
        let service = good_service(1);
        let pressure = CpuPressureSample {
            observed_at_ms: 1,
            some_ppm: 200_000,
        };
        let constrained = policy.evaluate(1, Some(&service), Some(&pressure)).unwrap();
        assert_eq!(constrained.state, State::Constrained);
        assert_eq!(constrained.proposed_quota_us, 2_000);
    }
}
