#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProcessMemory {
    pub bytes: u64,
}

impl ProcessMemory {
    #[cfg(feature = "desktop")]
    pub const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Process {
    pid: u32,
    parent: u32,
    rss: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn process_memory(pid: u32) -> Option<ProcessMemory> {
    use std::collections::{HashMap, HashSet};

    let mut root = None;
    let mut children: HashMap<u32, Vec<Process>> = HashMap::new();
    for process in processes()? {
        if process.pid == pid {
            root = Some(process);
        } else {
            children.entry(process.parent).or_default().push(process);
        }
    }

    let mut pending = vec![root?];
    let mut seen = HashSet::new();
    let mut bytes = 0_u64;
    while let Some(process) = pending.pop() {
        if seen.insert(process.pid) {
            bytes = bytes.saturating_add(process.rss);
            if let Some(descendants) = children.remove(&process.pid) {
                pending.extend(descendants);
            }
        }
    }
    Some(ProcessMemory { bytes })
}

#[cfg(target_os = "linux")]
fn processes() -> Option<Vec<Process>> {
    use std::fs;

    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let processes = fs::read_dir("/proc").ok()?;
    Some(
        processes
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let pid = entry.file_name().to_str()?.parse().ok()?;
                let stat = fs::read(entry.path().join("stat")).ok()?;
                // The command name can contain spaces, closing parentheses, and non-UTF-8 bytes.
                let end = stat.iter().rposition(|byte| *byte == b')')?;
                let mut fields = std::str::from_utf8(&stat[end + 1..])
                    .ok()?
                    .split_ascii_whitespace();
                if matches!(fields.next()?, "Z" | "X") {
                    return None;
                }
                let parent = fields.next()?.parse().ok()?;
                // State and PPID are fields 3 and 4. RSS is field 24 in pages.
                let pages = fields.nth(19)?.parse::<u64>().ok()?;
                let rss = pages.checked_mul(page_size)?;
                Some(Process { pid, parent, rss })
            })
            .collect(),
    )
}

#[cfg(target_os = "macos")]
fn processes() -> Option<Vec<Process>> {
    let output = crate::process::background("/bin/ps")
        .args(["-axo", "pid=,ppid=,rss=,state="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        std::str::from_utf8(&output.stdout)
            .ok()?
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_ascii_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let parent = fields.next()?.parse().ok()?;
                let rss = fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
                if fields.next()?.starts_with('Z') {
                    return None;
                }
                Some(Process { pid, parent, rss })
            })
            .collect(),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_memory(_: u32) -> Option<ProcessMemory> {
    None
}

#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
impl ProcessIdentity {
    pub(crate) fn current() -> Option<Self> {
        let pid = std::process::id();
        Some(Self {
            pid,
            start_time: start_time(pid)?,
        })
    }

    pub(crate) fn sample(&self) -> Option<ProcessMemory> {
        if start_time(self.pid)? != self.start_time {
            return None;
        }
        let memory = process_memory(self.pid)?;
        (start_time(self.pid)? == self.start_time).then_some(memory)
    }
}

#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The process name is parenthesized. starttime is the twentieth field after it.
    stat.rsplit_once(')')?
        .1
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()
}
