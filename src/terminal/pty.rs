use rust_i18n::t;

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::Path,
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
}

impl Process {
    pub(crate) fn start(
        shell: OsString,
        mode: ShellMode,
        root: &Path,
        size: PtySize,
        commands: mpsc::Receiver<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        let root = root.to_owned();
        Self::spawn(events, move |output, stopping| {
            run(&shell, mode, &root, size, commands, output, stopping)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn start_remote(
        ssh: crate::remote::SshSession,
        size: PtySize,
        commands: mpsc::Receiver<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        Self::spawn(events, move |output, stopping| {
            remote_run(ssh, size, commands, output, stopping).map_err(io::Error::other)
        })
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
    shell: &OsString,
    mode: ShellMode,
    root: &Path,
    size: PtySize,
    commands: mpsc::Receiver<Command>,
    events: &async_channel::Sender<Event>,
    stopping: &AtomicBool,
) -> io::Result<Option<u32>> {
    let (mut pty, reader, writer) = platform::Pty::open(shell, mode, root, size)?;
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
    size: PtySize,
    commands: mpsc::Receiver<Command>,
    output: &async_channel::Sender<Event>,
    stopping: &AtomicBool,
) -> anyhow::Result<Option<u32>> {
    use std::{collections::VecDeque, time::Instant};

    use crate::remote::{Bytes, PendingCall, Request, TerminalEvent};

    const MAX_PENDING_COMMANDS: usize = 8;
    const CHECK_INTERVAL: Duration = Duration::from_millis(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let id: u64 = ssh.call(Request::TerminalOpen(size))?;
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
