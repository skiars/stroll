use std::{
    io::{ErrorKind, Write},
    process::{Child, Command, Stdio},
};

use tempfile::TempPath;

use super::*;

const PREVIEW_SECONDS: u32 = 6;

#[derive(Clone, Copy)]
enum AudioFormat {
    Mp3,
    Wav,
    Flac,
}

impl AudioFormat {
    fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "mp3" => Some(Self::Mp3),
            "wav" => Some(Self::Wav),
            "flac" => Some(Self::Flac),
            _ => None,
        }
    }

    fn demuxer(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }
}

pub(super) fn is_audio(path: &Path) -> bool {
    AudioFormat::from_path(path).is_some()
}

pub(super) struct AudioPreview {
    task: Task<()>,
    player: Option<Player>,
}

impl Default for AudioPreview {
    fn default() -> Self {
        Self {
            task: Task::ready(()),
            player: None,
        }
    }
}

impl AudioPreview {
    pub(super) fn stop(&mut self) {
        self.task = Task::ready(());
        self.player = None;
    }
}

enum AudioSource {
    Local(PathBuf),
    Remote(TempPath),
}

impl AudioSource {
    fn read(repo: &Repository, path: &Path, format: AudioFormat) -> Result<Self> {
        if repo.ssh.is_none() {
            return Ok(Self::Local(repo.resolve(path)?));
        }
        let bytes = repo.bytes(path, true)?;
        let mut file = tempfile::Builder::new()
            .prefix("stroll-audio-")
            .suffix(&format!(".{}", format.demuxer()))
            .tempfile()?;
        file.write_all(&bytes)?;
        Ok(Self::Remote(file.into_temp_path()))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Local(path) => path,
            Self::Remote(path) => path.as_ref(),
        }
    }
}

struct Player {
    process: Child,
    // Keep the downloaded file alive until mpv has exited.
    _source: AudioSource,
}

impl Player {
    fn start(source: AudioSource, format: AudioFormat) -> Result<Self> {
        let process = playback_command(source.path(), format)
            .spawn()
            .map_err(|error| {
                if error.kind() == ErrorKind::NotFound {
                    anyhow::anyhow!("{}", t!("audio.mpv_missing"))
                } else {
                    anyhow::Error::from(error)
                }
            })?;
        Ok(Self {
            process,
            _source: source,
        })
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if !matches!(self.process.try_wait(), Ok(Some(_))) {
            _ = self.process.kill();
            _ = self.process.wait();
        }
    }
}

fn playback_command(path: &Path, format: AudioFormat) -> Command {
    let mut command = Command::new("mpv");
    command
        .args([
            "--no-config",
            "--load-scripts=no",
            "--autoload-files=no",
            "--access-references=no",
            "--vid=no",
            "--audio-display=no",
            "--force-window=no",
            "--terminal=no",
            "--demuxer=lavf",
        ])
        .arg(format!("--demuxer-lavf-format={}", format.demuxer()))
        .arg(format!("--length={PREVIEW_SECONDS}"))
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

impl Workspace {
    pub(super) fn preview_audio(&mut self, path: PathBuf, window: &Window, cx: &mut Context<Self>) {
        if self.repository_ui.opening.is_some() || self.confirming_close {
            return;
        }
        let (Some(repo), Some(format)) = (self.repository.clone(), AudioFormat::from_path(&path))
        else {
            return;
        };
        self.audio_preview.stop();
        let source_path = path.clone();
        let source =
            cx.background_spawn(async move { AudioSource::read(&repo, &source_path, format) });
        self.audio_preview.task = cx.spawn_in(window, async move |this, cx| {
            let source = source.await;
            let Ok(true) = this.update_in(cx, |this, window, cx| {
                match source.and_then(|source| Player::start(source, format)) {
                    Ok(player) => {
                        this.audio_preview.player = Some(player);
                        true
                    }
                    Err(error) => {
                        show_error(&path, error, window, cx);
                        false
                    }
                }
            }) else {
                return;
            };
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let Ok(true) = this.update_in(cx, |this, window, cx| {
                    let Some(player) = &mut this.audio_preview.player else {
                        return false;
                    };
                    let error = match player.process.try_wait() {
                        Ok(None) => return true,
                        Ok(Some(status)) if status.success() => None,
                        Ok(Some(status)) => Some(anyhow::anyhow!("{status}")),
                        Err(error) => Some(error.into()),
                    };
                    this.audio_preview.player = None;
                    if let Some(error) = error {
                        show_error(&path, error, window, cx);
                    }
                    false
                }) else {
                    break;
                };
            }
        });
    }
}

fn show_error(path: &Path, error: anyhow::Error, window: &mut Window, cx: &mut App) {
    toast::error(
        window,
        cx,
        format!(
            "{}\n{error:#}",
            t!("audio.preview_failed", path = file_name(path))
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[::core::prelude::v1::test]
    fn stopping_audio_preview_reaps_the_player_and_removes_the_download() -> Result<()> {
        let source = tempfile::NamedTempFile::new()?.into_temp_path();
        let path = source.to_path_buf();
        let process = Command::new("sleep").arg("30").spawn()?;
        let proc_path = Path::new("/proc").join(process.id().to_string());
        let mut preview = AudioPreview {
            task: Task::ready(()),
            player: Some(Player {
                process,
                _source: AudioSource::Remote(source),
            }),
        };
        assert!(proc_path.exists());
        assert!(path.exists());
        preview.stop();
        assert!(!proc_path.exists());
        assert!(!path.exists());
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn audio_preview_command_limits_playback_and_disables_windows() {
        for (file, demuxer) in [
            ("sample.MP3", "mp3"),
            ("sample.wav", "wav"),
            ("sample.flac", "flac"),
        ] {
            let path = Path::new(file);
            let command = playback_command(path, AudioFormat::from_path(path).unwrap());
            let args: Vec<_> = command.get_args().collect();
            assert!(
                args.iter()
                    .any(|arg| *arg == format!("--demuxer-lavf-format={demuxer}").as_str())
            );
            assert!(args.iter().any(|arg| *arg == "--length=6"));
            assert!(args.iter().any(|arg| *arg == "--force-window=no"));
            assert!(args.iter().any(|arg| *arg == "--vid=no"));
            assert!(args.iter().any(|arg| *arg == "--terminal=no"));
            assert_eq!(args[args.len() - 2], "--");
            assert_eq!(args[args.len() - 1], path.as_os_str());
        }
        assert!(!is_audio(Path::new("sample.png")));
    }
}
