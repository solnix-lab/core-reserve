use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

use rustix::{
    fs::{Mode, OFlags, open},
    time::{ClockId, clock_gettime},
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSample {
    pub observed_at_ms: u64,
    pub service_instance: String,
    pub offered_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub client_missed_requests: u64,
    pub p999_us: Option<u64>,
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
pub struct ClockIdentity {
    boot_id: String,
    time_namespace: Option<String>,
}

impl ClockIdentity {
    pub fn read() -> io::Result<Self> {
        let time_namespace = match fs::read_link("/proc/self/ns/time") {
            Ok(namespace) => Some(namespace.to_string_lossy().into_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                .trim()
                .to_owned(),
            time_namespace,
        })
    }
}

pub fn boot_time_us() -> u64 {
    let now = clock_gettime(ClockId::Boottime);
    now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1_000
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceWindow {
    schema_version: u32,
    clock: ClockIdentity,
    sample: ServiceSample,
}

impl ServiceWindow {
    pub fn new(sample: ServiceSample) -> io::Result<Self> {
        Ok(Self {
            schema_version: 1,
            clock: ClockIdentity::read()?,
            sample,
        })
    }

    pub fn read(path: &Path, clock: &ClockIdentity) -> io::Result<ServiceSample> {
        const MAX_WINDOW_BYTES: u64 = 16 * 1024;
        let file = File::from(open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?);
        if !file.metadata()?.is_file() {
            return Err(invalid("service telemetry must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_WINDOW_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_WINDOW_BYTES {
            return Err(invalid("service telemetry file is too large"));
        }
        let window: Self = serde_json::from_slice(&bytes)?;
        if window.schema_version != 1 || window.clock != *clock {
            return Err(invalid(
                "service telemetry has an incompatible schema, boot, or time namespace",
            ));
        }
        Ok(window.sample)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
