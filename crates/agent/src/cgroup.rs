use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use rustix::{
    fd::OwnedFd,
    fs::{self as linux, FlockOperation, Mode, OFlags, ResolveFlags},
};
use serde::Serialize;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const CGROUP2_SUPER_MAGIC: linux::FsWord = 0x6367_7270;

pub struct Cgroup {
    path: PathBuf,
    directory: OwnedFd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CpuMax {
    /// A missing quota is the kernel's unlimited `max` value.
    pub quota_us: Option<u64>,
    pub period_us: u64,
}

impl CpuMax {
    pub fn limited(quota_us: u64, period_us: u64) -> Self {
        Self {
            quota_us: Some(quota_us),
            period_us,
        }
    }

    pub fn quota_in_period(self, period_us: u64) -> Option<u64> {
        self.quota_us.map(|quota| {
            let scaled = u128::from(quota) * u128::from(period_us) / u128::from(self.period_us);
            u64::try_from(scaled).unwrap_or(u64::MAX)
        })
    }

    fn compare_bandwidth(self, other: Self) -> Ordering {
        match (self.quota_us, other.quota_us) {
            (Some(left), Some(right)) => (u128::from(left) * u128::from(other.period_us))
                .cmp(&(u128::from(right) * u128::from(self.period_us))),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
}

#[derive(Serialize)]
pub struct CpuStat {
    pub usage_usec: u64,
    pub nr_periods: Option<u64>,
    pub nr_throttled: Option<u64>,
    pub throttled_usec: Option<u64>,
}

#[derive(Serialize)]
pub struct CpuStatus<'a> {
    pub path: &'a Path,
    pub cpu_max: Option<CpuMax>,
    pub cpu_stat: CpuStat,
    pub pressure_some_total_usec: u64,
}

impl Cgroup {
    pub fn open(path: &Path) -> io::Result<Self> {
        let relative = path.strip_prefix(CGROUP_ROOT).map_err(|_| {
            invalid_input("cgroup paths must be absolute paths below /sys/fs/cgroup")
        })?;
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid_input(
                "cgroup paths cannot contain parent traversal",
            ));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let root = linux::open(CGROUP_ROOT, flags | OFlags::NOFOLLOW, Mode::empty())?;
        if linux::fstatfs(&root)?.f_type != CGROUP2_SUPER_MAGIC {
            return Err(invalid_input("/sys/fs/cgroup is not a cgroup v2 mount"));
        }
        let target = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let directory = linux::openat2(
            &root,
            target,
            flags,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|error| {
            io::Error::new(
                io::Error::from(error).kind(),
                format!("{}: {error}", path.display()),
            )
        })?;
        Ok(Self {
            path: Path::new(CGROUP_ROOT).join(relative),
            directory,
        })
    }

    pub fn status(&self) -> io::Result<CpuStatus<'_>> {
        let cpu_max = match self.cpu_max() {
            Ok(quota) => Some(quota),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(CpuStatus {
            path: &self.path,
            cpu_max,
            cpu_stat: self.cpu_stat()?,
            pressure_some_total_usec: self.pressure_some_total()?,
        })
    }

    pub fn cpu_stat(&self) -> io::Result<CpuStat> {
        let stats = self.read("cpu.stat")?;
        Ok(CpuStat {
            usage_usec: stat_field(&stats, "usage_usec")?
                .ok_or_else(|| invalid_data("cpu.stat missing usage_usec"))?,
            nr_periods: stat_field(&stats, "nr_periods")?,
            nr_throttled: stat_field(&stats, "nr_throttled")?,
            throttled_usec: stat_field(&stats, "throttled_usec")?,
        })
    }

    pub fn pressure_some_total(&self) -> io::Result<u64> {
        parse_pressure(&self.read("cpu.pressure")?)
    }

    pub fn cpu_max(&self) -> io::Result<CpuMax> {
        parse_cpu_max(&self.read("cpu.max")?)
    }

    pub fn lock_batch(&self, service: &Self) -> io::Result<Actuator<'_>> {
        if self.path.starts_with(&service.path) || service.path.starts_with(&self.path) {
            return Err(invalid_input("batch and service cgroups must be disjoint"));
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        let root_is_visible = mountinfo.lines().any(|line| {
            let Some((mount, fs)) = line.split_once(" - ") else {
                return false;
            };
            let mut fields = mount.split_ascii_whitespace().skip(3);
            fields.next() == Some("/")
                && fields.next() == Some(CGROUP_ROOT)
                && fs.starts_with("cgroup2 ")
        });
        if !root_is_visible {
            return Err(invalid_input(
                "cannot verify agent membership on a cgroup mount with a scoped root",
            ));
        }
        let membership = fs::read_to_string("/proc/self/cgroup")?;
        let current = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::/"))
            .ok_or_else(|| invalid_data("no unified cgroup membership for the agent"))?;
        let current = Path::new(CGROUP_ROOT).join(current);
        if current.starts_with(&self.path) {
            return Err(invalid_input("batch cgroup contains the agent"));
        }
        // Reopen the directory to give each actuator a separate lock lifetime.
        let lock = self.open_file(".", OFlags::RDONLY | OFlags::DIRECTORY)?;
        linux::flock(&lock, FlockOperation::NonBlockingLockExclusive)?;
        // Opening for writing verifies access without changing the current quota.
        self.open_file("cpu.max", OFlags::WRONLY)?;
        Ok(Actuator {
            group: self,
            _lock: lock,
        })
    }

    fn open_file(&self, name: &str, flags: OFlags) -> io::Result<File> {
        linux::openat(
            &self.directory,
            name,
            flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| {
            io::Error::new(
                io::Error::from(error).kind(),
                format!("{}/{name}: {error}", self.path.display()),
            )
        })
    }

    fn read(&self, name: &str) -> io::Result<String> {
        let mut contents = String::new();
        self.open_file(name, OFlags::RDONLY)?
            .read_to_string(&mut contents)?;
        Ok(contents)
    }
}

pub struct Actuator<'a> {
    group: &'a Cgroup,
    _lock: File,
}

impl Actuator<'_> {
    pub fn reconcile(&self, desired: CpuMax, can_increase: bool) -> io::Result<CpuMax> {
        self.update(desired, can_increase, None)
    }

    pub fn reconcile_from(
        &self,
        expected: CpuMax,
        desired: CpuMax,
        can_increase: bool,
    ) -> io::Result<CpuMax> {
        self.update(desired, can_increase, Some(expected))
    }

    fn update(
        &self,
        desired: CpuMax,
        can_increase: bool,
        expected: Option<CpuMax>,
    ) -> io::Result<CpuMax> {
        let current = self.group.cpu_max()?;
        if expected.is_some_and(|expected| current != expected) {
            return Err(io::Error::other("cpu.max changed during reconciliation"));
        }
        let ordering = current.compare_bandwidth(desired);
        if ordering == Ordering::Equal || (ordering == Ordering::Less && !can_increase) {
            return Ok(current);
        }
        let quota = desired
            .quota_us
            .ok_or_else(|| invalid_input("actuator requires a finite quota"))?;
        if quota < 1_000 {
            // A finite quota from a longer period can scale below the kernel's minimum.
            // Keep that limit until recovery can request a representable quota.
            if ordering == Ordering::Less {
                return Ok(current);
            }
            return Err(invalid_input(
                "requested bandwidth is below the kernel minimum for this period",
            ));
        }
        let value = format!("{quota} {}\n", desired.period_us);
        let written = self
            .group
            .open_file("cpu.max", OFlags::WRONLY)?
            .write(value.as_bytes())?;
        if written != value.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short write to cpu.max",
            ));
        }
        let actual = self.group.cpu_max()?;
        if actual != desired {
            return Err(io::Error::other(format!(
                "cpu.max readback differs from the requested value: {actual:?}"
            )));
        }
        Ok(actual)
    }
}

fn parse_cpu_max(value: &str) -> io::Result<CpuMax> {
    let mut words = value.split_ascii_whitespace();
    let quota_us = match words.next() {
        Some("max") => None,
        Some(quota) => Some(number(quota, "invalid cpu.max quota")?),
        None => return Err(invalid_data("cpu.max is empty")),
    };
    let period = words
        .next()
        .ok_or_else(|| invalid_data("cpu.max missing period"))?;
    let period_us = number(period, "invalid cpu.max period")?;
    if period_us == 0 || quota_us == Some(0) || words.next().is_some() {
        return Err(invalid_data("invalid cpu.max fields"));
    }
    Ok(CpuMax {
        quota_us,
        period_us,
    })
}

fn parse_pressure(value: &str) -> io::Result<u64> {
    value
        .lines()
        .find_map(|line| line.strip_prefix("some "))
        .and_then(|line| {
            line.split_ascii_whitespace()
                .find_map(|word| word.strip_prefix("total="))
        })
        .ok_or_else(|| invalid_data("cpu.pressure missing some total"))
        .and_then(|value| number(value, "invalid cpu.pressure total"))
}

fn stat_field(stats: &str, key: &str) -> io::Result<Option<u64>> {
    stats
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find_map(|(name, value)| {
            (name == key).then(|| number(value.trim(), "invalid cpu.stat counter"))
        })
        .transpose()
}

fn number(value: &str, message: &'static str) -> io::Result<u64> {
    value.parse().map_err(|_| invalid_data(message))
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_quota_and_compares_bandwidth_across_periods() {
        let unlimited = parse_cpu_max("max 100000\n").unwrap();
        let narrow = parse_cpu_max("1000 1000000\n").unwrap();
        let half_cpu = parse_cpu_max("50000 100000\n").unwrap();
        assert_eq!(unlimited.compare_bandwidth(half_cpu), Ordering::Greater);
        assert_eq!(narrow.quota_in_period(100_000), Some(100));
        assert_eq!(narrow.compare_bandwidth(half_cpu), Ordering::Less);
        assert_eq!(
            CpuMax::limited(25_000, 50_000).compare_bandwidth(half_cpu),
            Ordering::Equal
        );
        assert!(parse_cpu_max("25000 0").is_err());
        assert!(parse_cpu_max("25000 100000 extra").is_err());
    }

    #[test]
    fn distinguishes_some_pressure_from_full_and_missing_counters() {
        assert_eq!(
            parse_pressure("some avg10=1.00 total=113\nfull avg10=0.00 total=9\n").unwrap(),
            113
        );
        assert!(parse_pressure("full avg10=0.00 total=9\n").is_err());
        let stat = "usage_usec 27\nfuture_counter 1\nnr_throttled 3\n";
        assert_eq!(stat_field(stat, "nr_throttled").unwrap(), Some(3));
        assert_eq!(stat_field(stat, "nr_periods").unwrap(), None);
    }
}
