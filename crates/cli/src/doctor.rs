use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use rustix::fs::{self as linux_fs, Access, AtFlags, CWD, StatVfsMountFlags};
use serde::Serialize;

const CPU_SYSFS: &str = "/sys/devices/system/cpu";
// Linux UAPI filesystem identifier from linux/magic.h.
const CGROUP2_SUPER_MAGIC: linux_fs::FsWord = 0x6367_7270;

#[derive(Serialize)]
pub(crate) struct Manifest {
    schema_version: u32,
    corereserve_version: &'static str,
    build: Build,
    kernel: Kernel,
    cpu: Cpu,
    cgroup: Cgroup,
}

#[derive(Serialize)]
struct Build {
    rustc: &'static str,
    cargo: &'static str,
}

#[derive(Serialize)]
struct Kernel {
    release: String,
    build: String,
    architecture: String,
    btf_readable: bool,
    sched_ext_state: Option<String>,
}

#[derive(Serialize)]
struct Cpu {
    model: Option<String>,
    online: Option<String>,
    smt_active: Option<bool>,
    smt_control: Option<String>,
    topology: Vec<LogicalCpu>,
}

#[derive(Serialize)]
struct LogicalCpu {
    id: u32,
    package_id: Option<i32>,
    core_id: Option<i32>,
    thread_siblings: Option<String>,
    microcode: Option<String>,
    governor: Option<String>,
}

#[derive(Serialize)]
struct Cgroup {
    path: String,
    v2: bool,
    mount_read_only: bool,
    available_controllers: Option<Vec<String>>,
    enabled_child_controllers: Option<Vec<String>>,
    effective_cpus: Option<String>,
    cpu_max: Option<String>,
    cpu_pressure_readable: bool,
    cpu_max_write_access: bool,
    subtree_control_write_access: bool,
}

impl Manifest {
    pub(crate) fn collect(cgroup_path: &Path) -> io::Result<Self> {
        let uname = rustix::system::uname();
        Ok(Self {
            schema_version: 1,
            corereserve_version: env!("CARGO_PKG_VERSION"),
            build: Build {
                rustc: env!("CORERESERVE_BUILD_RUSTC"),
                cargo: env!("CORERESERVE_BUILD_CARGO"),
            },
            kernel: Kernel {
                release: uname.release().to_string_lossy().into_owned(),
                build: uname.version().to_string_lossy().into_owned(),
                architecture: uname.machine().to_string_lossy().into_owned(),
                btf_readable: fs::File::open("/sys/kernel/btf/vmlinux").is_ok(),
                sched_ext_state: read_text("/sys/kernel/sched_ext/state"),
            },
            cpu: Cpu::collect(),
            cgroup: Cgroup::collect(cgroup_path)?,
        })
    }

    pub(crate) fn write_summary(&self, mut out: impl Write) -> io::Result<()> {
        let cgroup = &self.cgroup;
        writeln!(
            out,
            "Linux {} ({})",
            self.kernel.release, self.kernel.architecture
        )?;
        writeln!(
            out,
            "CPU model: {}",
            self.cpu.model.as_deref().unwrap_or("unavailable")
        )?;
        writeln!(
            out,
            "Online CPUs: {}",
            self.cpu.online.as_deref().unwrap_or("unavailable")
        )?;
        writeln!(
            out,
            "SMT active: {}",
            match self.cpu.smt_active {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unavailable",
            }
        )?;
        writeln!(out, "Cgroup: {}", cgroup.path)?;
        writeln!(out, "Cgroup v2: {}", yes_no(cgroup.v2))?;
        writeln!(out, "Read-only mount: {}", yes_no(cgroup.mount_read_only))?;
        writeln!(
            out,
            "Available controllers: {}",
            controller_list(&cgroup.available_controllers)
        )?;
        writeln!(
            out,
            "Enabled child controllers: {}",
            controller_list(&cgroup.enabled_child_controllers)
        )?;
        writeln!(
            out,
            "Effective CPUs: {}",
            cgroup.effective_cpus.as_deref().unwrap_or("unavailable")
        )?;
        writeln!(
            out,
            "CPU pressure readable: {}",
            yes_no(cgroup.cpu_pressure_readable)
        )?;
        writeln!(
            out,
            "CPU quota (cpu.max): {}",
            cgroup.cpu_max.as_deref().unwrap_or("unavailable")
        )?;
        writeln!(
            out,
            "CPU quota write access: {}",
            yes_no(cgroup.cpu_max_write_access)
        )?;
        writeln!(
            out,
            "Subtree controller write access: {}",
            yes_no(cgroup.subtree_control_write_access)
        )?;
        writeln!(
            out,
            "Kernel BTF readable: {}",
            yes_no(self.kernel.btf_readable)
        )?;
        writeln!(
            out,
            "sched_ext: {}",
            self.kernel
                .sched_ext_state
                .as_deref()
                .unwrap_or("unavailable")
        )
    }
}

impl Cpu {
    fn collect() -> Self {
        let root = Path::new(CPU_SYSFS);
        let mut topology: Vec<_> = fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let id = name.to_str()?.strip_prefix("cpu")?.parse().ok()?;
                Some(LogicalCpu::collect(id, entry.path()))
            })
            .collect();
        topology.sort_by_key(|cpu| cpu.id);

        let model = read_text("/proc/cpuinfo").and_then(|info| {
            info.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| value.trim().to_owned())
            })
        });
        let smt_active = match read_text(root.join("smt/active")).as_deref() {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };

        Self {
            model,
            online: read_text(root.join("online")),
            smt_active,
            smt_control: read_text(root.join("smt/control")),
            topology,
        }
    }
}

impl LogicalCpu {
    fn collect(id: u32, path: PathBuf) -> Self {
        Self {
            id,
            package_id: read_text(path.join("topology/physical_package_id"))
                .and_then(|value| value.parse().ok()),
            core_id: read_text(path.join("topology/core_id")).and_then(|value| value.parse().ok()),
            thread_siblings: read_text(path.join("topology/thread_siblings_list")),
            microcode: read_text(path.join("microcode/version")),
            governor: read_text(path.join("cpufreq/scaling_governor")),
        }
    }
}

impl Cgroup {
    fn collect(path: &Path) -> io::Result<Self> {
        let path = fs::canonicalize(path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        if !fs::metadata(&path)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", path.display()),
            ));
        }
        let v2 = linux_fs::statfs(&path)?.f_type == CGROUP2_SUPER_MAGIC;
        let mount_read_only = linux_fs::statvfs(&path)?
            .f_flag
            .contains(StatVfsMountFlags::RDONLY);
        let cpu_max = path.join("cpu.max");
        let subtree_control = path.join("cgroup.subtree_control");

        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            v2,
            mount_read_only,
            available_controllers: v2
                .then(|| read_words(path.join("cgroup.controllers")))
                .flatten(),
            enabled_child_controllers: v2.then(|| read_words(&subtree_control)).flatten(),
            effective_cpus: v2
                .then(|| read_text(path.join("cpuset.cpus.effective")))
                .flatten(),
            cpu_max: v2.then(|| read_text(&cpu_max)).flatten(),
            cpu_pressure_readable: v2 && read_text(path.join("cpu.pressure")).is_some(),
            cpu_max_write_access: v2 && !mount_read_only && can_write(&cpu_max),
            subtree_control_write_access: v2 && !mount_read_only && can_write(&subtree_control),
        })
    }
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn read_words(path: impl AsRef<Path>) -> Option<Vec<String>> {
    read_text(path).map(|value| value.split_whitespace().map(str::to_owned).collect())
}

fn can_write(path: &Path) -> bool {
    linux_fs::accessat(CWD, path, Access::WRITE_OK, AtFlags::EACCESS).is_ok()
}

fn controller_list(controllers: &Option<Vec<String>>) -> String {
    match controllers.as_deref() {
        None => "unavailable".to_owned(),
        Some([]) => "none".to_owned(),
        Some(controllers) => controllers.join(" "),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
