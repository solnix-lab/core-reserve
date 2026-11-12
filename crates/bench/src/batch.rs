use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use clap::Args;
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant as TokioInstant, sleep_until};

use crate::{Result, Shutdown, cpu_work, write_json};

const CHECKPOINT_VERSION: u32 = 1;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Args, Serialize)]
pub struct Settings {
    /// Linux CPU list, such as 0-3,8.
    #[arg(long)]
    pub(crate) cpus: Option<String>,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=256))]
    workers: u32,
    /// CPU work per completed batch unit.
    #[arg(long, default_value_t = 100_000, value_parser = clap::value_parser!(u32).range(1..=10_000_000))]
    iterations: u32,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    duration_secs: u64,
    /// Save progress here and resume it on the next invocation.
    #[arg(long)]
    checkpoint: Option<PathBuf>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    settings: Settings,
    effective_cpus: Vec<usize>,
    stopped_by_signal: bool,
    elapsed_secs: f64,
    resumed_units: u64,
    completed_units: u64,
    completed_units_per_second: f64,
    completed_units_per_host_hour: f64,
}

#[derive(Deserialize, Serialize)]
struct Progress {
    schema_version: u32,
    iterations: u32,
    completed_units_by_worker: Vec<u64>,
}

pub(crate) async fn run(settings: Settings, cpus: Vec<usize>) -> Result<()> {
    let mut shutdown = Shutdown::new()?;
    let checkpoint = settings
        .checkpoint
        .as_deref()
        .map(Checkpoint::open)
        .transpose()?;
    let progress = match &checkpoint {
        Some(checkpoint) => checkpoint.load(&settings)?,
        None => Progress::empty(&settings),
    };
    let resumed_units: u64 = progress.completed_units_by_worker.iter().sum();
    let counts = Arc::new(
        progress
            .completed_units_by_worker
            .into_iter()
            .map(AtomicU64::new)
            .collect::<Vec<_>>(),
    );

    let start = Instant::now();
    let deadline = start + Duration::from_secs(settings.duration_secs);
    let mut workers = Workers::start(&counts, settings.iterations, deadline)?;
    writeln!(io::stderr().lock(), "batch ready")?;
    let mut next_save = TokioInstant::now() + CHECKPOINT_INTERVAL;
    let stopped_by_signal = loop {
        tokio::select! {
            _ = shutdown.wait() => break true,
            _ = sleep_until(deadline.into()) => break false,
            _ = sleep_until(next_save), if checkpoint.is_some() => {
                if let Some(checkpoint) = &checkpoint {
                    checkpoint.save(&Progress::snapshot(&settings, &counts))?;
                }
                next_save = TokioInstant::now() + CHECKPOINT_INTERVAL;
            }
        }
    };
    workers.stop()?;
    let elapsed_secs = start.elapsed().as_secs_f64();
    let progress = Progress::snapshot(&settings, &counts);
    if let Some(checkpoint) = &checkpoint {
        checkpoint.save(&progress)?;
    }
    let completed_units = progress.completed_units_by_worker.iter().sum::<u64>() - resumed_units;
    let completed_units_per_second = completed_units as f64 / elapsed_secs;
    write_json(
        io::stdout().lock(),
        &Report {
            schema_version: 1,
            settings,
            effective_cpus: cpus,
            stopped_by_signal,
            elapsed_secs,
            resumed_units,
            completed_units,
            completed_units_per_second,
            completed_units_per_host_hour: completed_units_per_second * 3_600.0,
        },
    )
}

struct Workers {
    stop_requested: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Workers {
    fn start(counts: &Arc<Vec<AtomicU64>>, iterations: u32, deadline: Instant) -> io::Result<Self> {
        let mut workers = Self {
            stop_requested: Arc::new(AtomicBool::new(false)),
            threads: Vec::with_capacity(counts.len()),
        };
        for id in 0..counts.len() {
            let stop_requested = Arc::clone(&workers.stop_requested);
            let counts = Arc::clone(counts);
            workers.threads.push(thread::Builder::new().spawn(move || {
                let count = &counts[id];
                let mut next = count.load(Relaxed);
                while !stop_requested.load(Relaxed) && Instant::now() < deadline {
                    let seed = next
                        .wrapping_mul(counts.len() as u64)
                        .wrapping_add(id as u64);
                    cpu_work(seed, iterations);
                    next += 1;
                    count.store(next, Relaxed);
                }
            })?);
        }
        Ok(workers)
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stop_requested.store(true, Relaxed);
        let mut panicked = false;
        for thread in self.threads.drain(..) {
            panicked |= thread.join().is_err();
        }
        if panicked {
            return Err(io::Error::other("a batch worker panicked"));
        }
        Ok(())
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Progress {
    fn empty(settings: &Settings) -> Self {
        Self {
            schema_version: CHECKPOINT_VERSION,
            iterations: settings.iterations,
            completed_units_by_worker: vec![0; settings.workers as usize],
        }
    }

    fn snapshot(settings: &Settings, counts: &[AtomicU64]) -> Self {
        Self {
            schema_version: CHECKPOINT_VERSION,
            iterations: settings.iterations,
            completed_units_by_worker: counts.iter().map(|count| count.load(Relaxed)).collect(),
        }
    }
}

struct Checkpoint {
    path: PathBuf,
    parent: PathBuf,
    _lock: File,
}

impl Checkpoint {
    fn open(path: &Path) -> Result<Self> {
        let name = path.file_name().ok_or("checkpoint requires a file name")?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        let path = parent.join(name);
        let mut lock_name = OsString::from(name);
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join(lock_name))?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
            io::Error::other(format!(
                "could not lock checkpoint {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self {
            path,
            parent,
            _lock: lock,
        })
    }

    fn load(&self, settings: &Settings) -> Result<Progress> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Progress::empty(settings));
            }
            Err(error) => return Err(error.into()),
        };
        let progress: Progress = serde_json::from_reader(BufReader::new(file))?;
        if progress.schema_version != CHECKPOINT_VERSION
            || progress.iterations != settings.iterations
            || progress.completed_units_by_worker.len() != settings.workers as usize
            || progress
                .completed_units_by_worker
                .iter()
                .any(|&count| count > u64::MAX / 512)
        {
            return Err("checkpoint has incompatible or invalid batch progress".into());
        }
        Ok(progress)
    }

    fn save(&self, progress: &Progress) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.parent)?;
        write_json(file.as_file_mut(), progress)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        File::open(&self.parent)?.sync_all()?;
        Ok(())
    }
}
