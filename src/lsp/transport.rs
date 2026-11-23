use std::{
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender, SyncSender, TrySendError},
    },
    thread,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{Event, LocalProcess};

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 4096;

pub(super) struct Transport {
    child: Child,
    local_process: Option<LocalProcess>,
    output: SyncSender<Value>,
}

impl Transport {
    pub(super) fn start(
        command: &mut Command,
        root: Option<&Path>,
        local_process: Option<LocalProcess>,
        events: Sender<Event>,
        stderr_tail: Arc<Mutex<Vec<u8>>>,
        stderr_done: Sender<()>,
    ) -> Result<Self> {
        if let Some(root) = root {
            command.current_dir(root);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut input = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut output = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let stderr = child.stderr.take().expect("piped stderr");
        let (outgoing, messages) = mpsc::sync_channel::<Value>(16);
        let transport = Self {
            child,
            local_process,
            output: outgoing,
        };

        thread::Builder::new()
            .name("lsp-stderr".into())
            .spawn(move || {
                let mut stderr = BufReader::new(stderr);
                while let Ok(bytes) = stderr.fill_buf() {
                    if bytes.is_empty() {
                        break;
                    }
                    let len = bytes.len();
                    {
                        let mut tail = stderr_tail.lock().unwrap();
                        let bytes = &bytes[len.saturating_sub(STDERR_TAIL_BYTES)..];
                        let excess = (tail.len() + bytes.len()).saturating_sub(STDERR_TAIL_BYTES);
                        tail.drain(..excess);
                        tail.extend_from_slice(bytes);
                    }
                    stderr.consume(len);
                }
                _ = stderr_done.send(());
            })?;

        let reader_events = events.clone();
        thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || {
                loop {
                    match read_message(&mut input) {
                        Ok(message) => {
                            if reader_events.send(Event::Incoming(message)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            _ = reader_events.send(Event::Disconnected(error));
                            break;
                        }
                    }
                }
            })?;
        thread::Builder::new()
            .name("lsp-writer".into())
            .spawn(move || {
                for message in messages {
                    if let Err(error) = write_message(&mut output, &message) {
                        _ = events.send(Event::Disconnected(error));
                        break;
                    }
                }
            })?;
        if let Some(process) = &transport.local_process {
            process.set_pid(transport.child.id());
        }
        Ok(transport)
    }

    pub(super) fn send(&self, message: Value) -> Result<()> {
        self.output
            .try_send(message)
            .context("The LSP output queue is unavailable")
    }
    pub(super) fn try_send(&self, message: Value) -> Result<bool> {
        match self.output.try_send(message) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(error) => Err(error).context("The LSP output queue is unavailable"),
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Some(process) = &self.local_process {
            process.set_pid(0);
        }
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

fn read_message(input: &mut impl BufRead) -> Result<Value> {
    let mut length = None;
    let mut line = String::new();
    let mut header_bytes = 0;
    loop {
        line.clear();
        let bytes = input.take(8192).read_line(&mut line)?;
        header_bytes += bytes;
        if bytes == 0 && header_bytes == 0 {
            bail!("The language server exited");
        }
        ensure!(
            bytes > 0 && header_bytes < 8192,
            "Invalid LSP message header"
        );
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("The LSP message is missing Content-Length")?;
    ensure!(length <= MAX_MESSAGE_BYTES, "The LSP message is too large");
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    Ok(output.flush()?)
}
