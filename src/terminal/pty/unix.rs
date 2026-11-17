use std::{
    env,
    ffi::{CStr, OsStr, OsString},
    fs::File,
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    ptr, thread,
    time::{Duration, Instant},
};

use super::PtySize;

pub(super) struct Pty {
    master: File,
    child: Option<Child>,
}

pub(crate) fn shell() -> OsString {
    if let Some(shell) = env::var_os("SHELL").filter(|value| !value.is_empty()) {
        return shell;
    }
    // getpwuid_r writes only to this thread's buffers.
    let mut account = MaybeUninit::uninit();
    let mut result = ptr::null_mut();
    let mut buffer = vec![0; 16 * 1024];
    unsafe {
        if libc::getpwuid_r(
            libc::getuid(),
            account.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        ) == 0
            && !result.is_null()
            && !(*result).pw_shell.is_null()
        {
            let shell = CStr::from_ptr((*result).pw_shell).to_bytes();
            if !shell.is_empty() {
                return OsStr::from_bytes(shell).to_owned();
            }
        }
    }
    "/bin/sh".into()
}

impl Pty {
    pub(super) fn open(
        shell: &OsStr,
        root: &Path,
        size: PtySize,
    ) -> io::Result<(Self, File, File)> {
        let mut master = -1;
        let mut slave = -1;
        let mut initial = winsize(size);
        // openpty returns two owned descriptors. The parent never keeps a slave open.
        unsafe {
            check(libc::openpty(
                &mut master,
                &mut slave,
                ptr::null_mut(),
                ptr::null_mut(),
                std::ptr::addr_of_mut!(initial),
            ))?;
        }
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            check(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        }
        let reader = master.try_clone()?;
        let writer = master.try_clone()?;
        let mut command = Command::new(shell);
        command
            .arg("-i")
            .current_dir(root)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // The pre-exec callback runs after stdio redirection and uses only syscalls.
        unsafe {
            command.pre_exec(|| {
                check(libc::setsid())?;
                check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        Ok((
            Self {
                master,
                child: Some(child),
            },
            reader,
            writer,
        ))
    }

    pub(super) fn resize(&mut self, size: PtySize) -> io::Result<()> {
        check(unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &winsize(size),
            )
        })?;
        Ok(())
    }

    pub(super) fn exited(&mut self) -> io::Result<Option<u32>> {
        let pid = self.child.as_ref().unwrap().id() as libc::pid_t;
        // Preserve the session leader PID until its jobs have been reclaimed.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = check(unsafe {
            libc::waitid(
                libc::P_PID,
                pid as _,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        });
        match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(None),
            result => {
                result?;
            }
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        let code = if info.si_code == libc::CLD_EXITED {
            unsafe { info.si_status() as u32 }
        } else {
            128 + unsafe { info.si_status() as u32 }
        };
        Ok(Some(code))
    }

    pub(super) fn close(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let sid = child.id() as libc::pid_t;
        signal_session(sid, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            if session_members(sid).all(|pid| pid == sid) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Jobs have separate process groups. The session ID includes both
        // the foreground process group and background jobs.
        signal_session(sid, libc::SIGKILL);
        _ = child.kill();
        _ = child.wait();
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn winsize(size: PtySize) -> libc::winsize {
    libc::winsize {
        ws_col: size.cols,
        ws_row: size.rows,
        ws_xpixel: size.cols.saturating_mul(size.cell_width),
        ws_ypixel: size.rows.saturating_mul(size.cell_height),
    }
}

fn signal_session(sid: libc::pid_t, signal: libc::c_int) {
    for pid in session_members(sid) {
        // Recheck membership immediately before sending a signal.
        unsafe {
            if libc::getsid(pid) == sid {
                libc::kill(pid, signal);
            }
        }
    }
}

fn session_members(sid: libc::pid_t) -> impl Iterator<Item = libc::pid_t> {
    process_ids()
        .into_iter()
        .filter(move |&pid| pid > 1 && unsafe { libc::getsid(pid) } == sid)
}

#[cfg(target_os = "linux")]
fn process_ids() -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

#[cfg(target_os = "macos")]
fn process_ids() -> Vec<libc::pid_t> {
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    let mut pids = vec![0; count.max(0) as usize + 64];
    let bytes = std::mem::size_of_val(pids.as_slice()) as libc::c_int;
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(count.max(0) as usize);
    pids
}
