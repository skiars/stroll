use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use crate::{
    process_memory::{ProcessMemory, process_memory},
    remote::{LSP_MEMORY_VERSION, Request, SshSession},
};
use std::{path::PathBuf, sync::Mutex};

#[derive(Clone, Default)]
pub(crate) struct LocalProcess(Arc<AtomicU32>);

impl PartialEq for LocalProcess {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl LocalProcess {
    pub(super) fn set_pid(&self, pid: u32) {
        self.0.store(pid, Ordering::Relaxed);
    }

    pub(super) fn is_running(&self) -> bool {
        self.0.load(Ordering::Relaxed) != 0
    }

    pub(crate) fn sample(&self) -> Option<ProcessMemory> {
        let pid = self.0.load(Ordering::Relaxed);
        if pid == 0 {
            return None;
        }
        let memory = process_memory(pid)?;
        // The transport clears the PID before reaping the child.
        self.is_running().then_some(memory)
    }
}

#[derive(Clone)]
pub(crate) struct RemoteProcess(Arc<RemoteProcessState>);

struct RemoteProcessState {
    ssh: SshSession,
    id: Mutex<Option<u64>>,
}

impl PartialEq for RemoteProcess {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl RemoteProcess {
    pub(crate) fn new(ssh: SshSession) -> Self {
        Self(Arc::new(RemoteProcessState {
            ssh,
            id: Mutex::new(None),
        }))
    }

    pub(crate) fn prepare(&self) -> Option<PathBuf> {
        let version = self
            .0
            .ssh
            .call::<u32>(Request::Hello(LSP_MEMORY_VERSION))
            .ok()?;
        if version != LSP_MEMORY_VERSION {
            return None;
        }
        let (id, path) = self.0.ssh.call(Request::RegisterLspMemory).ok()?;
        *self.0.id.lock().unwrap() = Some(id);
        Some(path)
    }

    pub(super) fn release(&self) {
        let id = self.0.id.lock().unwrap().take();
        if let Some(id) = id {
            _ = self.0.ssh.call::<()>(Request::ReleaseLspMemory(id));
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.0.id.lock().unwrap().is_some()
    }

    fn sample(&self) -> Option<ProcessMemory> {
        let id = (*self.0.id.lock().unwrap())?;
        self.0.ssh.call(Request::LspMemory(id)).ok().flatten()
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum LanguageProcess {
    Local(LocalProcess),
    Remote(RemoteProcess),
}

impl LanguageProcess {
    pub(crate) fn sample(&self) -> Option<ProcessMemory> {
        match self {
            Self::Local(process) => process.sample(),
            Self::Remote(process) => process.sample(),
        }
    }
}
