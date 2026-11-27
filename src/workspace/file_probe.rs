use crate::repository::{FILE_KIND_BATCH, probe_binary_files};

use super::*;

#[derive(Default)]
pub(super) struct FileProbes {
    pub(super) checked: Rc<RefCell<HashSet<PathBuf>>>,
    queued: HashSet<PathBuf>,
    running: Vec<PathBuf>,
    task: Option<Task<()>>,
}

impl FileProbes {
    pub(super) fn invalidate(&mut self, changed: impl Fn(&Path) -> bool) {
        self.checked.borrow_mut().retain(|path| !changed(path));
        if self.running.iter().any(|path| changed(path)) {
            self.task = None;
            self.queued.extend(self.running.drain(..));
        }
    }
}

impl Workspace {
    pub(super) fn probe_visible_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let probes = &mut self.file_probes;
        // Prefer the newest visible rows while a previous batch is in flight.
        probes.queued = paths
            .into_iter()
            .filter(|path| {
                !probes.checked.borrow().contains(path)
                    && !probes.running.contains(path)
                    && !self.file_formats.contains_key(path)
            })
            .collect();
        self.start_file_probe(cx);
    }

    fn start_file_probe(&mut self, cx: &mut Context<Self>) {
        let probes = &mut self.file_probes;
        if probes.task.is_some() || probes.queued.is_empty() {
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let paths = probes
            .queued
            .iter()
            .take(FILE_KIND_BATCH)
            .cloned()
            .collect::<Vec<_>>();
        for path in &paths {
            probes.queued.remove(path);
        }
        probes.running.clone_from(&paths);
        let load = cx.background_spawn(async move {
            let matches = probe_binary_files(Some(&repo), &paths);
            (paths, matches)
        });
        probes.task = Some(cx.spawn(async move |this, cx| {
            let (paths, matches) = load.await;
            _ = this.update(cx, |this, cx| {
                let probes = &mut this.file_probes;
                probes.checked.borrow_mut().extend(paths.iter().cloned());
                probes.running.clear();
                probes.task = None;
                let mut updated = false;
                if let Ok(matches) = matches {
                    let formats = Rc::make_mut(&mut this.file_formats);
                    for (path, binary) in paths.into_iter().zip(matches) {
                        if binary
                            && let std::collections::hash_map::Entry::Vacant(entry) =
                                formats.entry(path)
                        {
                            entry.insert(file_icons::KnownFormat::Binary);
                            updated = true;
                        }
                    }
                }
                if updated {
                    cx.notify();
                }
                this.start_file_probe(cx);
            });
        }));
    }
}
