use std::{ffi::OsStr, process::Command};

pub(crate) fn background(program: impl AsRef<OsStr>) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new(program);
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    Command::new(program)
}

#[cfg(unix)]
pub(crate) fn login_shell(shell: &OsStr, directory: &std::path::Path, flags: &str) -> Command {
    let mut command = Command::new(shell);
    command
        .args([flags, r"printf '\0stroll-env\0'; exec env -0"])
        .current_dir(directory)
        .stdin(std::process::Stdio::null());
    command
}

#[cfg(unix)]
pub(crate) fn parse_login_environment(
    output: &[u8],
) -> anyhow::Result<std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>> {
    use anyhow::{Context as _, ensure};
    use std::{collections::HashMap, os::unix::ffi::OsStrExt as _};

    // Startup files can write to stdout before the environment is printed.
    let marker = b"\0stroll-env\0";
    let start = output
        .windows(marker.len())
        .position(|part| part == marker)
        .context("The login shell did not return environment variables")?
        + marker.len();
    let mut environment = HashMap::new();
    for entry in output[start..].split(|&byte| byte == 0) {
        if let Some(separator) = entry.iter().position(|&byte| byte == b'=') {
            environment.insert(
                OsStr::from_bytes(&entry[..separator]).to_owned(),
                OsStr::from_bytes(&entry[separator + 1..]).to_owned(),
            );
        }
    }
    ensure!(
        environment.contains_key(OsStr::new("PATH")),
        "The login shell did not provide PATH"
    );
    Ok(environment)
}
