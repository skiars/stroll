use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{App, BorrowAppContext as _, Context, Global, Task, Window};
use notify::{RecursiveMode, Watcher as _};

use crate::config::ConfigFile;

use super::Workspace;

#[derive(Default)]
struct ConfigSources(HashMap<PathBuf, ConfigSource>);

impl Global for ConfigSources {}

struct ConfigSource {
    file: ConfigFile,
    revision: u64,
    _watch: Task<()>,
}

pub(super) fn subscribe(
    file: ConfigFile,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> ConfigFile {
    let Ok(path) = file.path().map(|path| path.to_owned()) else {
        return file;
    };
    if !cx.has_global::<ConfigSources>() {
        cx.set_global(ConfigSources::default());
    }
    if !cx.global::<ConfigSources>().0.contains_key(&path) {
        let watch = watch_config(path.clone(), cx);
        cx.update_global::<ConfigSources, _>(|sources, _| {
            sources.0.insert(
                path.clone(),
                ConfigSource {
                    file,
                    revision: 0,
                    _watch: watch,
                },
            );
        });
    }
    let file = cx.global::<ConfigSources>().0[&path].file.clone();
    cx.observe_global_in::<ConfigSources>(window, move |workspace, window, cx| {
        let next = &cx.global::<ConfigSources>().0[&path].file;
        if !workspace.config.same_disk_state(next) {
            let previous = std::mem::replace(&mut workspace.config, next.clone());
            workspace.apply_config(&previous, window, cx);
        }
    })
    .detach();
    file
}

pub(super) fn publish(file: &ConfigFile, cx: &mut App) {
    let Ok(path) = file.path() else { return };
    let Some(source) = cx
        .try_global::<ConfigSources>()
        .and_then(|sources| sources.0.get(path))
    else {
        return;
    };
    if source.file.same_disk_state(file) {
        return;
    }
    cx.update_global::<ConfigSources, _>(|sources, _| {
        let source = sources.0.get_mut(path).unwrap();
        source.file = file.clone();
        source.revision += 1;
    });
}

fn watch_config(path: PathBuf, cx: &App) -> Task<()> {
    let changed = Arc::new(AtomicBool::new(false));
    let signal = changed.clone();
    let target = path.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.as_ref().map_or(true, |event| {
            event.need_rescan() || !event.kind.is_access() && event.paths.contains(&target)
        }) {
            signal.store(true, Ordering::Relaxed);
        }
    })
    .ok();
    cx.spawn(async move |cx| {
        let mut watching = false;
        loop {
            if !watching && let (Some(watcher), Some(parent)) = (&mut watcher, path.parent()) {
                watching = watcher.watch(parent, RecursiveMode::NonRecursive).is_ok();
            }
            let (mut next, revision) = cx.update(|cx| {
                let source = &cx.global::<ConfigSources>().0[&path];
                (source.file.clone(), source.revision)
            });
            let background = cx.background_executor().clone();
            next = background
                .spawn(async move {
                    let _ = next.reload();
                    next
                })
                .await;
            cx.update(|cx| {
                if cx.global::<ConfigSources>().0[&path].revision == revision {
                    publish(&next, cx);
                }
            });

            let changed = changed.clone();
            background
                .clone()
                .spawn(async move {
                    // Keep filesystem callbacks off the UI scheduler. The timeout
                    // also catches symlink writes and a replaced parent directory.
                    for _ in 0..20 {
                        background.timer(Duration::from_millis(100)).await;
                        if changed.swap(false, Ordering::Relaxed) {
                            background.timer(Duration::from_millis(120)).await;
                            changed.store(false, Ordering::Relaxed);
                            break;
                        }
                    }
                })
                .await;
        }
    })
}
