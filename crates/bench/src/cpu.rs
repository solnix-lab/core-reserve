use std::io;

use rustix::thread::{CpuSet, sched_getaffinity, sched_setaffinity};

pub(crate) fn apply_affinity(requested: Option<&str>) -> io::Result<Vec<usize>> {
    let original = sched_getaffinity(None)?;
    let Some(requested) = requested else {
        return Ok(cpu_ids(&original));
    };
    let mask = parse_mask(requested)?;
    let cpus = cpu_ids(&mask);
    if let Some(cpu) = cpus.iter().find(|&&cpu| !original.is_set(cpu)) {
        return Err(invalid(format!(
            "CPU {cpu} is outside the current affinity mask"
        )));
    }
    sched_setaffinity(None, &mask)?;
    if sched_getaffinity(None)? != mask {
        return Err(invalid(
            "the kernel did not apply the full requested CPU mask",
        ));
    }
    Ok(cpus)
}

fn parse_mask(value: &str) -> io::Result<CpuSet> {
    let mut mask = CpuSet::new();
    for part in value.split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let parse = |id: &str| {
            id.parse::<usize>()
                .ok()
                .filter(|&id| id < CpuSet::MAX_CPU)
                .ok_or_else(|| invalid(format!("invalid CPU ID {id:?}")))
        };
        let (first, last) = (parse(first)?, parse(last)?);
        if first > last {
            return Err(invalid(format!("invalid CPU range {part:?}")));
        }
        for id in first..=last {
            mask.set(id);
        }
    }
    Ok(mask)
}

fn cpu_ids(mask: &CpuSet) -> Vec<usize> {
    (0..CpuSet::MAX_CPU).filter(|&id| mask.is_set(id)).collect()
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
