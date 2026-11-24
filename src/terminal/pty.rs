use rust_i18n::t;

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[cfg(unix)]
#[path = "pty/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "pty/windows.rs"]
mod platform;

pub(crate) use platform::shell;

#[cfg(feature = "desktop")]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::atomic::AtomicU32;

#[derive(Clone, Copy)]
pub(crate) enum ShellMode {
    #[cfg(feature = "desktop")]
    Interactive,
    Login,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PtySize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

pub(crate) enum Command {
    Input(Vec<u8>),
    Resize(PtySize),
}

pub(crate) enum Event {
    Output(Vec<u8>),
    Exit(u32),
    Error(String),
}

pub(crate) struct Process {
    stop: Arc<AtomicBool>,
    events: async_channel::Sender<Event>,
    worker: Option<JoinHandle<()>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pid: Option<Arc<AtomicU32>>,
    #[cfg(feature = "desktop")]
    remote_directory: Option<Arc<Mutex<Option<PathBuf>>>>,
}

impl Process {
    pub(crate) fn start(
        shell: OsString,
        mode: ShellMode,
        root: &Path,
        directory: Option<&Path>,
        size: PtySize,
        commands: mpsc::Receiver<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        Self::start_in(shell, mode, root, directory, size, commands, events)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn start_local(
        shell: OsString,
        root: &Path,
        directory: Option<&Path>,
        size: PtySize,
        commands: mpsc::Receiver<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        Self::start_in(
            shell,
            ShellMode::Interactive,
            root,
            directory,
            size,
            commands,
            events,
        )
    }

    fn start_in(
        shell: OsString,
        mode: ShellMode,
        root: &Path,
        directory: Option<&Path>,
        size: PtySize,
        commands: mpsc::Receiver<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        let root = root.to_owned();
        let directory = directory.map(Path::to_owned);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let pid = Arc::new(AtomicU32::new(0));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let child_pid = pid.clone();
        let process = Self::spawn(events, move |output, stopping| {
            let pty = if let Some(directory) = directory.as_deref().filter(|path| path != &root) {
                platform::Pty::open(&shell, mode, directory, size, stopping)
                    .or_else(|_| platform::Pty::open(&shell, mode, &root, size, stopping))?
            } else {
                platform::Pty::open(&shell, mode, &root, size, stopping)?
            };
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            child_pid.store(pty.0.pid(), Ordering::Relaxed);
            let result = run(pty, size, commands, output, stopping);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            child_pid.store(0, Ordering::Relaxed);
            result
        })?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let process = {
            let mut process = process;
            process.pid = Some(pid);
            process
        };
        Ok(process)
    }

    pub(crate) fn working_directory(&self) -> Option<PathBuf> {
        #[cfg(feature = "desktop")]
        if let Some(directory) = &self.remote_directory {
            return directory.lock().unwrap().clone();
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let pid = self.pid.as_ref()?.load(Ordering::Relaxed);
            (pid != 0)
                .then(|| platform::working_directory(pid))
                .flatten()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        None
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn start_remote(
        ssh: crate::remote::SshSession,
        directory: Option<PathBuf>,
        size: PtySize,
        commands: mpsc::Receiver<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        use crate::remote::{Request, TERMINAL_DIRECTORY_VERSION};

        let reported = Arc::new(Mutex::new(None));
        let current = reported.clone();
        let mut process = Self::spawn(events, move |output, stopping| {
            let supports_directory = ssh
                .call::<u32>(Request::Hello(TERMINAL_DIRECTORY_VERSION))
                .is_ok_and(|version| version == TERMINAL_DIRECTORY_VERSION);
            let request = if supports_directory {
                Request::TerminalOpenAt(size, directory)
            } else {
                Request::TerminalOpen(size)
            };
            let id = ssh.call(request).map_err(io::Error::other)?;
            remote_run(ssh, id, &current, commands, output, stopping).map_err(io::Error::other)
        })?;
        process.remote_directory = Some(reported);
        Ok(process)
    }

    fn spawn(
        events: async_channel::Sender<Event>,
        run: impl FnOnce(&async_channel::Sender<Event>, &AtomicBool) -> io::Result<Option<u32>>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let output = events.clone();
        let worker = thread::Builder::new()
            .name("terminal-pty".into())
            .spawn(move || {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                let result = run(&output, &stopping);
                if !stopping.load(Ordering::Relaxed) {
                    let event = match result {
                        Ok(Some(code)) => Event::Exit(code),
                        Ok(None) => return,
                        Err(error) => Event::Error(error.to_string()),
                    };
                    _ = output.send_blocking(event);
                }
            })?;
        Ok(Self {
            stop,
            events,
            worker: Some(worker),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pid: None,
            #[cfg(feature = "desktop")]
            remote_directory: None,
        })
    }

    pub(crate) fn terminate(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.events.close();
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
        if let Some(worker) = self.worker.take() {
            _ = worker.join();
        }
    }
}

fn run(
    (mut pty, reader, writer): (
        platform::Pty,
        impl Read + Send + 'static,
        impl Write + Send + 'static,
    ),
    size: PtySize,
    commands: mpsc::Receiver<Command>,
    events: &async_channel::Sender<Event>,
    stopping: &AtomicBool,
) -> io::Result<Option<u32>> {
    // Keep ConPTY's synchronous input and output pipes on separate threads.
    // Output is bounded so an unbounded producer cannot starve the UI or fill memory.
    let reader_stop = Arc::new(AtomicBool::new(false));
    let output_stopping = reader_stop.clone();
    let output = events.clone();
    let reader = match thread::Builder::new()
        .name("terminal-output".into())
        .spawn(move || {
            read_output(reader, &output, &output_stopping);
        }) {
        Ok(reader) => reader,
        Err(error) => {
            pty.close();
            return Err(error);
        }
    };
    let (input, input_rx) = mpsc::channel::<Vec<u8>>();
    let writer = thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || {
            write_input(writer, input_rx);
        });

    let result = if writer.is_ok() {
        drive(&mut pty, size, commands, &input, stopping)
    } else {
        Err(io::Error::other(t!("terminal.input_thread_failed")))
    };

    drop(input);
    if stopping.load(Ordering::Relaxed) {
        reader_stop.store(true, Ordering::Relaxed);
    }
    pty.close();
    _ = reader.join();
    if let Ok(writer) = writer {
        _ = writer.join();
    }
    result
}

fn drive(
    pty: &mut platform::Pty,
    mut size: PtySize,
    commands: mpsc::Receiver<Command>,
    input: &mpsc::Sender<Vec<u8>>,
    stopping: &AtomicBool,
) -> io::Result<Option<u32>> {
    while !stopping.load(Ordering::Relaxed) {
        if let Some(code) = pty.exited()? {
            return Ok(Some(code));
        }
        match commands.recv_timeout(Duration::from_millis(15)) {
            Ok(Command::Input(bytes)) => {
                _ = input.send(bytes);
            }
            Ok(Command::Resize(mut next)) => {
                // Coalesce drag updates without reordering them past input.
                loop {
                    match commands.try_recv() {
                        Ok(Command::Resize(latest)) => next = latest,
                        Ok(Command::Input(bytes)) => {
                            if size != next {
                                pty.resize(next)?;
                                size = next;
                            }
                            _ = input.send(bytes);
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return Ok(None),
                    }
                }
                if size != next {
                    pty.resize(next)?;
                    size = next;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(None)
}

fn read_output(
    mut reader: impl Read,
    events: &async_channel::Sender<Event>,
    stopping: &AtomicBool,
) {
    let mut buffer = [0; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) if !stopping.load(Ordering::Relaxed) => {
                if events
                    .send_blocking(Event::Output(buffer[..count].to_vec()))
                    .is_err()
                {
                    continue;
                }
            }
            Ok(_) => {} // Keep draining ConPTY until ClosePseudoConsole returns.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => {
                if !stopping.load(Ordering::Relaxed) {
                    _ = events.send_blocking(Event::Error(error.to_string()));
                }
                break;
            }
        }
    }
}

fn write_input(mut writer: impl Write, commands: mpsc::Receiver<Vec<u8>>) {
    while let Ok(bytes) = commands.recv() {
        if writer.write_all(&bytes).is_err() {
            break;
        }
    }
}

#[cfg(feature = "desktop")]
fn remote_run(
    ssh: crate::remote::SshSession,
    id: u64,
    directory: &Mutex<Option<PathBuf>>,
    commands: mpsc::Receiver<Command>,
    output: &async_channel::Sender<Event>,
    stopping: &AtomicBool,
) -> anyhow::Result<Option<u32>> {
    use std::{collections::VecDeque, time::Instant};

    use crate::remote::{Bytes, PendingCall, Request, TerminalEvent};

    const MAX_PENDING_COMMANDS: usize = 8;
    const CHECK_INTERVAL: Duration = Duration::from_millis(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let result = (|| {
        // The SSH writer and remote terminal worker both process requests in order.
        let mut pending = VecDeque::<PendingCall>::new();
        let mut next_command = None;
        let mut poll = None::<PendingCall>;
        let mut next_poll = Instant::now();
        loop {
            if stopping.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if let Some(call) = &poll
                && let Some(events) = call.try_reply::<Vec<TerminalEvent>>()?
            {
                poll = None;
                for event in events {
                    match event {
                        TerminalEvent::Output(Bytes(bytes)) => {
                            if output.send_blocking(Event::Output(bytes)).is_err() {
                                return Ok(None);
                            }
                        }
                        TerminalEvent::Exit(code) => return Ok(Some(code)),
                        TerminalEvent::Error(error) => anyhow::bail!("{error}"),
                        TerminalEvent::Directory(path) => *directory.lock().unwrap() = Some(path),
                    }
                }
            }
            while let Some(call) = pending.front() {
                if call.try_reply::<()>()?.is_none() {
                    break;
                }
                pending.pop_front();
            }
            if next_command.is_none() && pending.len() < MAX_PENDING_COMMANDS {
                next_command = match commands.recv_timeout(CHECK_INTERVAL) {
                    Ok(Command::Input(bytes)) => Some(Request::TerminalInput(id, Bytes(bytes))),
                    Ok(Command::Resize(size)) => Some(Request::TerminalResize(id, size)),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                };
            } else {
                thread::sleep(CHECK_INTERVAL);
            }
            if let Some(request) = next_command.take() {
                match ssh.try_terminal_call(request)? {
                    Ok(call) => pending.push_back(call),
                    Err(request) => next_command = Some(request),
                }
            }
            if poll.is_none() && Instant::now() >= next_poll {
                if let Ok(call) = ssh.try_terminal_call(Request::TerminalPoll(id))? {
                    poll = Some(call);
                    next_poll = Instant::now() + POLL_INTERVAL;
                }
            }
        }
    })();
    _ = ssh.call::<()>(Request::TerminalClose(id));
    result
}

#[cfg(all(test, unix, feature = "desktop"))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, time::Instant};

    use super::*;

    #[test]
    fn local_terminal_uses_the_login_environment_in_an_interactive_shell() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let shell = root.join("shell");
        fs::write(
            &shell,
            r#"#!/bin/sh
if [ "$1" = -lc ]; then
    printf 'Login startup output\n'
    export PATH="$PWD/tools:$PATH"
    export STROLL_TERMINAL_TEST_LOGIN=loaded
    exec /bin/sh -c "$2"
fi
[ "$1" = -i ] && [ -t 0 ] || exit 2
exec /bin/sh "$@"
"#,
        )?;
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755))?;
        fs::create_dir(root.join("tools"))?;
        let tool = root.join("tools/stroll-terminal-path-test");
        fs::write(
            &tool,
            "#!/bin/sh\n[ \"$STROLL_TERMINAL_TEST_LOGIN\" = loaded ]\n",
        )?;
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755))?;

        let (tx, rx) = mpsc::channel();
        let (output, events) = async_channel::bounded(16);
        let _process = Process::start_local(
            shell.into_os_string(),
            &root,
            None,
            PtySize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            rx,
            output,
        )?;
        tx.send(Command::Input(
            b"stroll-terminal-path-test; exit $?\n".to_vec(),
        ))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(Event::Exit(code)) => {
                    assert_eq!(code, 0);
                    return Ok(());
                }
                Ok(Event::Error(error)) => anyhow::bail!("{error}"),
                Ok(Event::Output(_)) => continue,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        anyhow::bail!("Terminal did not exit")
    }

    #[test]
    fn terminal_starts_in_saved_directory_and_falls_back_if_removed() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let saved = root.join("saved");
        let next = root.join("next");
        fs::create_dir(&saved)?;
        fs::create_dir(&next)?;
        let start = |directory: &Path| -> anyhow::Result<_> {
            let (tx, rx) = mpsc::channel();
            let (output, events) = async_channel::bounded(16);
            let process = Process::start_local(
                "/bin/sh".into(),
                &root,
                Some(directory),
                PtySize {
                    cols: 80,
                    rows: 24,
                    cell_width: 8,
                    cell_height: 16,
                },
                rx,
                output,
            )?;
            Ok((process, tx, events))
        };
        let wait_for = |process: &Process, directory: &Path| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while process.working_directory().as_deref() != Some(directory) {
                assert!(
                    Instant::now() < deadline,
                    "expected {directory:?}, got {:?}",
                    process.working_directory()
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        let (process, tx, _events) = start(&saved)?;
        wait_for(&process, &saved);
        tx.send(Command::Input(b"cd ../next\n".to_vec()))?;
        wait_for(&process, &next);
        drop(process);

        fs::remove_dir(&saved)?;
        let (process, _tx, _events) = start(&saved)?;
        wait_for(&process, &root);
        Ok(())
    }
}
