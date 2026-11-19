#[cfg(any(feature = "desktop", test))]
use std::collections::HashMap;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabSession {
    paths: Vec<PathBuf>,
    active: Option<PathBuf>,
    preview: Option<PathBuf>,
}

impl TabSession {
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub fn active(&self) -> Option<&Path> {
        self.active.as_deref()
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn deselect(&mut self) -> Option<PathBuf> {
        self.active.take()
    }

    pub fn is_preview(&self, path: &Path) -> bool {
        self.preview.as_deref() == Some(path)
    }

    pub fn index(&self, path: &Path) -> Option<usize> {
        self.paths.iter().position(|tab| tab == path)
    }

    /// Opens or selects a file and returns the previous preview if it was replaced.
    pub fn open(&mut self, path: PathBuf, preview: bool) -> Option<PathBuf> {
        let mut replaced = None;
        if self.index(&path).is_none() {
            if preview {
                if let Some(index) = self.preview.as_deref().and_then(|path| self.index(path)) {
                    replaced = Some(std::mem::replace(&mut self.paths[index], path.clone()));
                } else {
                    self.paths.push(path.clone());
                }
                self.preview = Some(path.clone());
            } else {
                self.paths.push(path.clone());
            }
        } else if !preview {
            self.keep(&path);
        }
        self.active = Some(path);
        replaced
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn open_in_background(&mut self, path: PathBuf) {
        if self.index(&path).is_none() {
            self.paths.push(path);
        } else {
            self.keep(&path);
        }
    }

    pub fn keep(&mut self, path: &Path) -> bool {
        if !self.is_preview(path) {
            return false;
        }
        self.preview = None;
        true
    }

    pub fn close(&mut self, path: &Path) {
        let Some(index) = self.index(path) else {
            return;
        };
        self.paths.remove(index);
        self.keep(path);
        if self.active() == Some(path) {
            self.active = self.paths.get(index).or_else(|| self.paths.last()).cloned();
        }
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn remap(&mut self, map: impl Fn(&Path) -> PathBuf) {
        self.paths = self.paths.iter().map(|path| map(path)).collect();
        self.active = self.active.as_deref().map(&map);
        self.preview = self.preview.as_deref().map(&map);
        self.restore(|_| true);
    }

    pub(crate) fn restore(&mut self, mut exists: impl FnMut(&Path) -> bool) {
        let mut seen = BTreeSet::new();
        let active_index = self.active().and_then(|path| self.index(path));
        let mut fallback = 0;
        let mut index = 0;
        self.paths.retain(|path| {
            let keep = exists(path) && seen.insert(path.clone());
            if keep && active_index.is_some_and(|active| index < active) {
                fallback += 1;
            }
            index += 1;
            keep
        });
        if self.active().is_none_or(|path| self.index(path).is_none()) {
            self.active = self
                .paths
                .get(fallback)
                .or_else(|| self.paths.last())
                .cloned();
        }
        if self
            .preview
            .as_deref()
            .is_some_and(|path| self.index(path).is_none())
        {
            self.preview = None;
        }
    }
}

#[cfg(any(feature = "desktop", test))]
pub(crate) fn labels(paths: &[PathBuf]) -> Vec<String> {
    let mut groups = HashMap::<_, Vec<_>>::new();
    for path in paths {
        groups.entry(path.file_name()).or_default().push(path);
    }
    paths
        .iter()
        .map(|path| {
            let group = &groups[&path.file_name()];
            let mut suffix = PathBuf::from(path.file_name().unwrap_or_default());
            if group.len() > 1 {
                for component in path
                    .parent()
                    .into_iter()
                    .flat_map(|parent| parent.components().rev())
                {
                    suffix = Path::new(component.as_os_str()).join(suffix);
                    if group
                        .iter()
                        .filter(|other| other.ends_with(&suffix))
                        .take(2)
                        .count()
                        == 1
                    {
                        break;
                    }
                }
            }
            crate::paths::display(&suffix)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_labels_use_the_shortest_distinct_parent_suffix() {
        let paths = [
            "index.ts",
            "client/index.ts",
            "a/src/index.ts",
            "b/src/index.ts",
            "README.md",
        ]
        .map(PathBuf::from);
        assert_eq!(
            labels(&paths),
            paths.map(|path| path
                .components()
                .collect::<PathBuf>()
                .to_string_lossy()
                .into_owned())
        );
        let paths = ["a/client/main.ts", "b/server/main.ts"].map(PathBuf::from);
        assert_eq!(
            labels(&paths),
            ["client", "server"].map(|dir| Path::new(dir)
                .join("main.ts")
                .to_string_lossy()
                .into_owned())
        );
    }
}
