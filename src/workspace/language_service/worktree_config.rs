use super::*;
use crate::{
    language::worktree::CONFIG_PATH, remote::Request, repository::Repository, workspace::toast,
};

pub(in crate::workspace) struct WorktreeLanguageState {
    reload: Task<()>,
    prompted: bool,
    configured: bool,
    pub(super) error: Option<String>,
    pub(super) retiring: Vec<(String, bool, async_channel::Receiver<()>)>,
}

impl Default for WorktreeLanguageState {
    fn default() -> Self {
        Self {
            reload: Task::ready(()),
            prompted: false,
            configured: false,
            error: None,
            retiring: Vec::new(),
        }
    }
}

impl Workspace {
    pub(in crate::workspace) fn refresh_worktree_language(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repository.clone().filter(|_| self.startup_lsp.enabled) else {
            return;
        };
        let trusted = self.session.worktree_lsp_trusted;
        let previous = repo.worktree_lsp.read().unwrap().clone();
        let defaults = self.startup_lsp.clone();
        let paths: Vec<_> = self
            .files
            .keys()
            .filter(|path| !path.is_absolute())
            .cloned()
            .collect();
        let reading = {
            let repo = repo.clone();
            cx.background_spawn(async move {
                let config = read_config(&repo, &defaults);
                let next = match &config {
                    Ok(config) if trusted => Some(config.clone()),
                    Ok(_) => Some(WorktreeLsp::default()),
                    Err(_) if trusted => None,
                    Err(_) => Some(WorktreeLsp::default()),
                };
                let update = next.filter(|next| next != previous.as_ref()).map(|next| {
                    let projects = paths
                        .into_iter()
                        .map(|path| {
                            let project = ServerProject::with_config(&repo, &path, &next);
                            (path, project)
                        })
                        .collect::<Vec<_>>();
                    (Arc::new(next), projects)
                });
                (config, update)
            })
        };
        self.worktree_language.reload = cx.spawn_in(window, async move |this, cx| {
            let (config, update) = reading.await;
            _ = this.update_in(cx, |this, window, cx| {
                if !this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                    || this.session.worktree_lsp_trusted != trusted
                {
                    return;
                }
                match config {
                    Ok(config) => {
                        this.worktree_language.error = None;
                        this.worktree_language.configured = !config.is_empty();
                        if !trusted && !config.is_empty() && !this.worktree_language.prompted {
                            this.prompt_worktree_language_trust(window, cx);
                        }
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        if this.worktree_language.error.as_ref() != Some(&error) {
                            toast::error(window, cx, error.clone());
                            this.worktree_language.error = Some(error);
                        }
                    }
                }
                if let Some((next, projects)) = update {
                    let resolved: HashSet<_> =
                        projects.iter().map(|(path, _)| path.clone()).collect();
                    *repo.worktree_lsp.write().unwrap() = next.clone();
                    this.apply_worktree_projects(&next, projects, cx);
                    let new_files: Vec<_> = this
                        .files
                        .keys()
                        .filter(|path| !path.is_absolute() && !resolved.contains(*path))
                        .cloned()
                        .collect();
                    for path in new_files {
                        this.resolve_worktree_language_file(&path, cx);
                    }
                }
                if this.worktree_language.configured && this.repository_watch_pending {
                    this.repository_watch_pending = false;
                    this.watch_repository(window, cx);
                }
                cx.notify();
            });
        });
    }

    fn prompt_worktree_language_trust(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        self.worktree_language.prompted = true;
        let prompt = window.prompt(
            PromptLevel::Warning,
            &t!("lsp.worktree_trust_title"),
            Some(&t!(
                "lsp.worktree_trust_detail",
                root = paths::display(&repo.root)
            )),
            &[
                PromptButton::new(t!("common.cancel")),
                PromptButton::new(t!("lsp.trust_worktree")),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if prompt.await.ok() == Some(1) {
                _ = this.update_in(cx, |this, window, cx| {
                    if this
                        .repository
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &repo))
                    {
                        this.set_worktree_language_trust(true, window, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn set_worktree_language_trust(
        &mut self,
        trusted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.worktree_lsp_trusted = trusted;
        self.session_revision += 1;
        self.refresh_worktree_language(window, cx);
    }

    pub(super) fn worktree_language_controls(
        &self,
        workspace: WeakEntity<Self>,
        colors: Palette,
    ) -> Option<impl IntoElement> {
        let trusted = self.session.worktree_lsp_trusted;
        if !self.worktree_language.configured && !trusted && self.worktree_language.error.is_none()
        {
            return None;
        }
        Some(
            v_flex()
                .gap_2()
                .when_some(self.worktree_language.error.clone(), |panel, error| {
                    panel.child(last_error(error, colors))
                })
                .when(self.worktree_language.configured || trusted, |panel| {
                    panel.child(
                        action_button("worktree-language-trust")
                            .small()
                            .secondary()
                            .label(if trusted {
                                t!("lsp.revoke_worktree_trust")
                            } else {
                                t!("lsp.trust_worktree")
                            })
                            .on_click(move |_, window, cx| {
                                _ = workspace.update(cx, |this, cx| {
                                    if trusted {
                                        this.set_worktree_language_trust(false, window, cx);
                                    } else {
                                        this.prompt_worktree_language_trust(window, cx);
                                    }
                                });
                            }),
                    )
                }),
        )
    }

    pub(in crate::workspace) fn resolve_worktree_language_file(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let path = path.to_owned();
        let config = repo.worktree_lsp.read().unwrap().clone();
        let resolving = {
            let repo = repo.clone();
            let config = config.clone();
            let path = path.clone();
            cx.background_spawn(async move { ServerProject::with_config(&repo, &path, &config) })
        };
        cx.spawn(async move |this, cx| {
            let project = resolving.await;
            _ = this.update(cx, |this, cx| {
                if !this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                    || !Arc::ptr_eq(&config, &repo.worktree_lsp.read().unwrap())
                {
                    return;
                }
                this.apply_worktree_projects(&config, vec![(path, project)], cx);
            });
        })
        .detach();
    }

    fn apply_worktree_projects(
        &mut self,
        config: &WorktreeLsp,
        projects: Vec<(PathBuf, Option<ServerProject>)>,
        cx: &mut Context<Self>,
    ) {
        let mut previous = HashSet::new();
        let mut changed = Vec::new();
        for (path, project) in projects {
            if let Some(file) = self.files.get_mut(&path)
                && file.server_project != project
            {
                if let Some(old) = &file.server_project {
                    previous.insert(old.key.clone());
                }
                file.server_project = project;
                changed.push(path);
            }
        }
        let in_use: HashSet<_> = self
            .files
            .values()
            .filter_map(|file| {
                file.server_project
                    .as_ref()
                    .map(|project| project.key.clone())
            })
            .collect();
        self.language_servers.retain(|key, service| {
            let obsolete = match &key.server {
                ServiceServer::Worktree(server) => {
                    config.entry(&server.0.entry.id).is_none_or(|entry| {
                        !entry.enabled() || entry.fingerprint != server.0.fingerprint
                    })
                }
                _ => false,
            };
            if !in_use.contains(key) && (obsolete || previous.contains(key)) {
                if let LanguageService::Session { client, .. } = service {
                    client.stop();
                    self.worktree_language.retiring.push((
                        key.server_id().to_owned(),
                        key.remote,
                        client.stopped(),
                    ));
                }
                false
            } else {
                true
            }
        });
        for path in changed {
            let key = self.files[&path]
                .server_project
                .as_ref()
                .map(|project| project.key.clone());
            if let Some(key) = key.filter(|key| !self.language_servers.contains_key(key)) {
                self.start_language_service(&key, cx);
            } else {
                self.connect_file_to_language_service(&path, cx);
            }
        }
    }
}

pub(in crate::workspace) fn read_config(
    repo: &Repository,
    defaults: &crate::config::Lsp,
) -> anyhow::Result<WorktreeLsp> {
    if let Some(ssh) = &repo.ssh {
        if ssh.supports_worktree_lsp() {
            return ssh.call(Request::WorktreeLspConfig);
        }
        if let Some(bytes) = repo.bytes_if_present(Path::new(CONFIG_PATH))?
            && crate::language::worktree::has_entries(&bytes)
        {
            anyhow::bail!("{}", t!("lsp.worktree_helper_upgrade"));
        }
        return Ok(WorktreeLsp::default());
    }
    WorktreeLsp::load(&repo.root, |server| {
        let (command, source) = server
            .local_command(&defaults.server(server).command)
            .expect("startup LSP configuration was validated");
        (command, matches!(source, CommandSource::Path))
    })
}
