use std::{
    env,
    ffi::{OsStr, OsString, c_void},
    fs::File,
    io,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    ptr,
};

use super::{PtySize, ShellMode};

#[repr(C)]
struct Handles {
    console: *mut c_void,
    job: *mut c_void,
    process: *mut c_void,
}

unsafe extern "C" {
    fn reader_conpty_open(
        pty: *mut Handles,
        input: *mut *mut c_void,
        output: *mut *mut c_void,
        command: *mut u16,
        directory: *const u16,
        cols: u16,
        rows: u16,
    ) -> u32;
    fn reader_conpty_resize(pty: *mut Handles, cols: u16, rows: u16) -> u32;
    fn reader_conpty_exited(pty: *mut Handles, status: *mut u32) -> i32;
    fn reader_conpty_close(pty: *mut Handles);
}

pub(super) struct Pty(Option<Handles>);

pub(crate) fn shell() -> OsString {
    env::var_os("COMSPEC")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cmd.exe".into())
}

impl Pty {
    pub(super) fn open(
        shell: &OsStr,
        _mode: ShellMode,
        root: &Path,
        size: PtySize,
        _stopping: &std::sync::atomic::AtomicBool,
    ) -> io::Result<(Self, File, File)> {
        let mut handles = Handles {
            console: ptr::null_mut(),
            job: ptr::null_mut(),
            process: ptr::null_mut(),
        };
        let mut input = ptr::null_mut();
        let mut output = ptr::null_mut();
        let mut command = vec![b'"' as u16];
        command.extend(shell.encode_wide());
        command.extend([b'"' as u16, 0]);
        // Shells may treat a verbatim path as a device path. Use the conventional
        // form when it still names the same directory and fits in MAX_PATH.
        let conventional = crate::paths::simplified(root);
        let directory = if conventional != root
            && conventional.as_os_str().encode_wide().count() < 260
            && conventional.canonicalize().is_ok_and(|path| path == root)
        {
            conventional.as_path()
        } else {
            root
        };
        let directory: Vec<_> = directory.as_os_str().encode_wide().chain([0]).collect();
        let error = unsafe {
            reader_conpty_open(
                &mut handles,
                &mut input,
                &mut output,
                command.as_mut_ptr(),
                directory.as_ptr(),
                size.cols,
                size.rows,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        // ConPTY returns owned, non-inheritable pipe handles on success.
        Ok(unsafe {
            (
                Self(Some(handles)),
                File::from_raw_handle(output),
                File::from_raw_handle(input),
            )
        })
    }

    pub(super) fn resize(&mut self, size: PtySize) -> io::Result<()> {
        let error = unsafe { reader_conpty_resize(self.0.as_mut().unwrap(), size.cols, size.rows) };
        if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        }
    }

    pub(super) fn exited(&mut self) -> io::Result<Option<u32>> {
        let mut code = 0;
        let exited = unsafe { reader_conpty_exited(self.0.as_mut().unwrap(), &mut code) } != 0;
        Ok(exited.then_some(code))
    }

    pub(super) fn close(&mut self) {
        if let Some(mut handles) = self.0.take() {
            unsafe { reader_conpty_close(&mut handles) };
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}
