//! End-to-end stdio protocol tests for `stroll-remote`.
//!
//! Send frames to a real helper to cover workspace operations and terminal startup.

use std::{
    io::{BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};

struct Helper {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Helper {
    fn start() -> Self {
        Self::start_with(&mut Command::new(env!("CARGO_BIN_EXE_stroll-remote")))
    }

    fn start_with(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            next_id: 1,
        }
    }

    fn call(&mut self, request: Value) -> Value {
        let frame = json!({ "id": self.next_id, "request": request }).to_string();
        self.next_id += 1;
        self.input
            .write_all(&(frame.len() as u32).to_be_bytes())
            .unwrap();
        self.input.write_all(frame.as_bytes()).unwrap();
        self.input.flush().unwrap();
        loop {
            let mut reply = self.read_message();
            if reply["id"] == self.next_id - 1 {
                return reply["result"].take();
            }
            assert_eq!(reply["id"], 0, "{reply}");
        }
    }

    fn read_message(&mut self) -> Value {
        let mut length = [0; 4];
        self.output.read_exact(&mut length).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
        self.output.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn ok(&mut self, request: Value) -> Value {
        let result = self.call(request);
        assert!(result.get("Ok").is_some(), "Request failed: {result}");
        result["Ok"].clone()
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(root.join("b with space.txt"), "alpha\nbeta\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(root.join("b with space.txt"), "alpha\nBETA\n").unwrap();
    repo
}

fn comparison() -> Value {
    json!({ "Worktree": { "mode": "All", "base": null } })
}

#[test]
fn helper_edits_commit_history_and_requires_confirmation_for_published_commits() {
    let repo = fixture();
    let root = repo.path();
    git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 25})), 25);
    helper.ok(json!({"OpenWorkspace": root}));
    let history = helper.ok(json!("History"));
    let tip = json!({"branch": history["checkout"], "oid": history["head"]});
    let state = helper.ok(json!("CommitState"));
    assert_eq!(
        helper.ok(json!({"HistoryAmendMessage": [tip, state]})),
        "initial"
    );
    let discarded_plan = helper.ok(json!({"HistoryEditPlan": [tip, [tip["oid"]]]}));
    let edit = json!({"Amend": [tip, state, "New title"]});
    assert_eq!(
        helper.ok(json!({"EditHistory": [edit, false]})),
        "PublishedHistory"
    );
    assert_eq!(helper.ok(json!("History"))["head"], tip["oid"]);

    let completed = helper.ok(json!({"EditHistory": [edit, true]}));
    let head = &completed["Completed"][0];
    let rewrite = &completed["Completed"][1];
    assert_ne!(*head, tip["oid"]);
    assert_eq!(helper.ok(json!({"Commit": head}))["subject"], "New title");
    assert_eq!(
        helper.ok(json!({"HistoryRewriteRecord": [tip["branch"], head]})),
        *rewrite
    );
    assert!(
        helper
            .call(json!({"EditHistory": [{"Rebase": discarded_plan}, true]}))
            .get("Err")
            .is_some()
    );
    helper.ok(json!({"EditHistory": [{"UndoRewrite": rewrite}, false]}));
    assert_eq!(helper.ok(json!("History"))["head"], tip["oid"]);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn helper_can_resume_a_cherry_pick_after_reconnecting() {
    let repo = fixture();
    let root = repo.path();
    let connect = || {
        let mut helper = Helper::start();
        helper.ok(json!({"OpenWorkspace": root}));
        helper
    };
    let mut helper = connect();
    let base = helper.ok(json!("History"))["head"].clone();
    git(root, &["switch", "-qc", "source"]);
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "source"]);
    let source = helper.ok(json!("History"))["head"].clone();
    git(root, &["switch", "-q", "main"]);
    std::fs::write(root.join("a.txt"), "one\nother\nthree\n").unwrap();
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "main"]);
    let history = helper.ok(json!("History"));
    let tip = json!({"branch": history["checkout"], "oid": history["head"]});
    assert_eq!(helper.ok(json!({"CanCherryPick": [tip, [source]]})), true);
    assert!(
        helper
            .call(json!({"EditHistory": [{"CherryPick": [tip, [source]]}, false]}))
            .get("Err")
            .is_some()
    );
    drop(helper);

    let mut helper = connect();
    let progress = helper.ok(json!("HistoryEditProgress"));
    assert_eq!(progress["commit"], source);
    assert_eq!(progress["conflicts"], json!(["a.txt"]));
    assert_eq!(helper.ok(json!("RecoverHistoryBackup")), false);
    std::fs::write(root.join("a.txt"), "resolved\n").unwrap();
    helper.ok(json!({"EditHistory": [{"Resume": [progress, {"Stage": "a.txt"}]}, false]}));
    let progress = helper.ok(json!("HistoryEditProgress"));
    assert_eq!(progress["conflicts"], json!([]));
    let completed = helper.ok(json!({"EditHistory": [{"Resume": [progress, "Continue"]}, false]}));
    assert!(helper.ok(json!("HistoryEditProgress")).is_null());
    let head = &completed["Completed"][0];
    let tip = json!({"branch": "refs/heads/main", "oid": head});
    let reset = helper.ok(json!({"HistoryResetPlan": [tip, base]}));
    assert_eq!(reset["removed"], 2);
    assert_eq!(
        std::fs::read_to_string(root.join("b with space.txt")).unwrap(),
        "alpha\nBETA\n"
    );
}

#[test]
fn helper_finds_a_moved_or_removed_linked_worktree_by_its_identity() {
    let repo = fixture();
    let root = repo.path().canonicalize().unwrap();
    let checkouts = tempfile::tempdir().unwrap();
    let original = checkouts.path().join("original");
    let moved = checkouts.path().join("moved worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-qb",
            "review",
            original.to_str().unwrap(),
        ],
    );

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 29})), 29);
    let repository = helper.ok(json!({"OpenWorkspace": original}));
    let identity = helper.ok(json!("WorktreeIdentity"));
    let locate = json!({"LocateWorktree": {
        "root": original,
        "common": repository["common_dir"],
        "id": identity["id"]
    }});
    assert_eq!(helper.ok(locate.clone())["root"], json!(original));
    git(
        &root,
        &[
            "worktree",
            "move",
            original.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    let location = helper.ok(locate.clone());
    assert_eq!(location["root"], json!(moved));
    assert_eq!(location["branch"], "review");
    assert_eq!(location["same_worktree"], true);

    let mut reopened = Helper::start();
    assert_eq!(reopened.ok(locate.clone()), location);
    reopened.ok(json!({"OpenWorkspace": moved}));
    assert_eq!(reopened.ok(json!("WorktreeIdentity")), identity);
    git(&root, &["worktree", "remove", moved.to_str().unwrap()]);
    let location = reopened.ok(locate);
    assert_eq!(location["root"], json!(root));
    assert_eq!(location["same_worktree"], false);
    assert_eq!(
        reopened.ok(json!({"WorktreeMarker": [identity["admin"], identity["id"]]})),
        false
    );
}

#[test]
fn helper_imports_a_tree_without_publishing_partial_files() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::fs;

    let repo = fixture();
    let root = repo.path();
    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 27})), 27);
    helper.ok(json!({"OpenWorkspace": root}));
    let id = helper.ok(json!({"BeginImport": "imported tree"}));
    helper.ok(json!({"ImportEntry": [id, "entry", "Directory"]}));
    let path = "entry/source.bin";
    helper.ok(json!({"ImportEntry": [id, path, {"File": {"executable": false}}]}));
    let bytes = vec![0xff; 1024 * 1024];
    assert!(
        helper
            .call(json!({"ImportChunk": [id, path, 1, "YQ=="]}))
            .get("Err")
            .is_some()
    );
    helper.ok(json!({"ImportChunk": [id, path, 0, STANDARD.encode(&bytes)]}));
    helper.ok(json!({"ImportChunk": [id, path, bytes.len(), "YQ=="]}));
    assert!(!root.join("imported tree").exists());
    helper.ok(json!({"FinishImport": [id, true]}));
    let expected = [bytes.as_slice(), b"a"].concat();
    assert_eq!(
        fs::read(root.join("imported tree/source.bin")).unwrap(),
        expected
    );

    let id = helper.ok(json!({"BeginImport": "collision.txt"}));
    helper.ok(json!({"ImportEntry": [id, "entry", {"File": {"executable": false}}]}));
    fs::write(root.join("collision.txt"), "created externally").unwrap();
    assert!(
        helper
            .call(json!({"FinishImport": [id, true]}))
            .get("Err")
            .is_some()
    );
    assert_eq!(
        fs::read_to_string(root.join("collision.txt")).unwrap(),
        "created externally"
    );

    let id = helper.ok(json!({"BeginImport": "aborted"}));
    helper.ok(json!({"ImportEntry": [id, "entry", "Directory"]}));
    assert!(
        helper
            .call(json!({"ImportEntry": [id, "entry/.git", "Directory"]}))
            .get("Err")
            .is_some()
    );
    helper.ok(json!({"FinishImport": [id, false]}));
    assert!(!root.join("aborted").exists());
    assert!(
        fs::read_dir(root)
            .unwrap()
            .flatten()
            .all(|entry| { !entry.file_name().to_string_lossy().starts_with(".tmp") })
    );
}

#[cfg(target_os = "linux")]
#[test]
fn helper_samples_only_the_registered_language_server() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        thread,
        time::{Duration, Instant},
    };

    let repo = fixture();
    let root = repo.path().canonicalize().unwrap();
    let shell = root.join("test-login-shell");
    fs::write(
        &shell,
        "#!/bin/sh\nprintf '\\0stroll-env\\0PATH=%s\\0STROLL_RUST_ANALYZER=%s\\0' \"$PATH\" \"$TEST_LSP_SERVER\"\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let executable = root.join("test-language-server");
    fs::write(&executable, "#!/bin/sh\nread command\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 28})), 28);
    helper.ok(json!({"OpenWorkspace": root}));
    let registration = helper.ok(json!("RegisterLspMemory"));
    let query = json!({"LspMemory": registration[0]});
    assert!(helper.ok(query.clone()).is_null());
    let options = URL_SAFE_NO_PAD.encode(
        json!({
            "repository": root, "project": root, "server": "Rust", "memory": registration[1],
        })
        .to_string(),
    );
    let mut process = Command::new(env!("CARGO_BIN_EXE_stroll-remote"))
        .args(["--lsp", &options])
        .env("SHELL", shell)
        .env("TEST_LSP_SERVER", executable)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while helper.ok(query.clone())["bytes"]
        .as_u64()
        .is_none_or(|bytes| bytes == 0)
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "Language server memory was unavailable"
        );
        thread::sleep(Duration::from_millis(20));
    }
    drop(process.stdin.take());
    process.wait().unwrap();
    assert!(helper.ok(query.clone()).is_null());
    helper.ok(json!({"ReleaseLspMemory": registration[0]}));
    assert!(helper.ok(query).is_null());
}

#[cfg(unix)]
#[test]
fn helper_opens_a_login_shell_on_a_pty() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        thread,
        time::{Duration, Instant},
    };

    let repo = fixture();
    let shell = repo.path().join("test-shell");
    fs::write(
        &shell,
        "#!/bin/sh\n[ \"$1\" = -l ] && [ -t 0 ] && [ -t 1 ] && [ -t 2 ]\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let mut helper =
        Helper::start_with(Command::new(env!("CARGO_BIN_EXE_stroll-remote")).env("SHELL", shell));
    helper.ok(json!({ "OpenWorkspace": repo.path() }));
    let terminal = helper.ok(json!({ "TerminalOpen": {
        "cols": 80, "rows": 24, "cell_width": 8, "cell_height": 16
    }}));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = helper.ok(json!({ "TerminalPoll": terminal }));
        let events = events.as_array().unwrap();
        if let Some(exit) = events.iter().find_map(|event| event.get("Exit")) {
            assert_eq!(*exit, 0, "Expected a login shell with terminal descriptors");
            break;
        }
        assert!(
            events.iter().all(|event| event.get("Error").is_none()),
            "{events:?}"
        );
        assert!(Instant::now() < deadline, "Terminal did not exit");
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn helper_reports_and_restores_the_terminal_directory() {
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };

    let repo = fixture();
    let root = repo.path().canonicalize().unwrap();
    let start = root.join("start");
    let changed = root.join("changed");
    fs::create_dir(&start).unwrap();
    fs::create_dir(&changed).unwrap();
    let mut helper = Helper::start_with(
        Command::new(env!("CARGO_BIN_EXE_stroll-remote")).env("SHELL", "/bin/sh"),
    );
    assert_eq!(helper.ok(json!({"Hello": 26})), 26);
    helper.ok(json!({"OpenWorkspace": root}));
    let size = json!({"cols": 80, "rows": 24, "cell_width": 8, "cell_height": 16});
    let terminal = helper.ok(json!({"TerminalOpenAt": [size, "start"]}));
    let directory = |helper: &mut Helper, terminal: &Value, expected: &Path| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = helper.ok(json!({"TerminalPoll": terminal}));
            let events = events.as_array().unwrap();
            assert!(
                events.iter().all(|event| event.get("Error").is_none()),
                "{events:?}"
            );
            if events
                .iter()
                .any(|event| event["Directory"] == json!(expected))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "The terminal did not report {expected:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    directory(&mut helper, &terminal, &start);
    helper.ok(json!({"TerminalInput": [terminal, "Y2QgLi4vY2hhbmdlZAo="]})); // cd ../changed
    directory(&mut helper, &terminal, &changed);
    helper.ok(json!({"TerminalClose": terminal}));

    let terminal = helper.ok(json!({"TerminalOpenAt": [size, "missing"]}));
    directory(&mut helper, &terminal, &root);
    helper.ok(json!({"TerminalClose": terminal}));
}

#[test]
fn helper_opens_a_workspace_and_returns_batched_patches() {
    let repo = fixture();
    let mut helper = Helper::start();
    helper.ok(json!({ "Hello": 1 }));
    // The workspace path contains backslashes on Windows.
    let opened = helper.ok(json!({ "OpenWorkspace": repo.path().to_string_lossy() }));
    assert!(
        opened["root"].as_str().is_some(),
        "OpenWorkspace should return the canonical root: {opened}"
    );

    let changes = helper.ok(json!({ "Changes": comparison() }));
    let files = changes["files"].as_array().expect("files");
    assert_eq!(files.len(), 2, "{changes}");

    let batch = helper.ok(json!({
        "FileChangesBatch": [
            ["a.txt", { "mode": "All", "context": 3, "whitespace": false }],
            ["a.txt", { "mode": "All", "context": 0, "whitespace": false }],
            ["b with space.txt", { "mode": "All", "context": 3, "whitespace": false }],
        ]
    }));
    let batch = batch.as_array().expect("batch");
    assert_eq!(batch.len(), 3);
    assert_eq!(batch[0]["layers"][0]["layer"], json!("Unstaged"));
    assert!(
        batch[0]["layers"][0]["patch"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("+TWO")),
        "{:?}",
        batch[0]
    );
    // Paths containing spaces must be extracted correctly from the grouped diff.
    assert!(
        batch[2]["layers"][0]["patch"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("+BETA")),
        "{:?}",
        batch[2]
    );
    // Both ends use the same version object, which the client uses for validation.
    assert_eq!(batch[0]["version"], batch[1]["version"]);
}

#[test]
fn helper_stage_invalidates_cached_state() {
    let repo = fixture();
    let mut helper = Helper::start();
    helper.ok(json!({ "Hello": 1 }));
    helper.ok(json!({ "OpenWorkspace": repo.path().to_string_lossy() }));

    let changes = helper.ok(json!({ "Changes": comparison() }));
    let mut expected = serde_json::Map::new();
    for file in changes["files"].as_array().expect("files") {
        expected.insert(
            file["path"].as_str().unwrap().to_owned(),
            file["worktree"].clone(),
        );
    }
    assert_eq!(expected.len(), 2);

    let before = helper.ok(json!({ "FileVersion": "a.txt" }));
    helper.ok(json!({ "Stage": [expected, ["a.txt"], true] }));
    let after = helper.ok(json!({ "FileVersion": "a.txt" }));
    assert_ne!(
        before["index"], after["index"],
        "The index snapshot must change after staging"
    );
    assert_eq!(after["status"]["code"], json!("M"));
    assert_eq!(after["status"]["staged"], json!(true));

    // Fetch the changes again. Only the unstaged layer should remain.
    let changes = helper.ok(json!({ "Changes": comparison() }));
    let file = changes["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["path"] == json!("a.txt"))
        .expect("a.txt");
    assert_eq!(file["worktree"]["unstaged"], json!(false));
}

#[test]
fn helper_rejects_opening_a_second_workspace() {
    let repo = fixture();
    let mut helper = Helper::start();
    helper.ok(json!({ "Hello": 1 }));
    helper.ok(json!({ "OpenWorkspace": repo.path().to_string_lossy() }));
    let second = helper.call(json!({ "OpenWorkspace": repo.path().to_string_lossy() }));
    assert!(second.get("Err").is_some(), "{second}");
}

#[test]
fn helper_streams_worktree_and_index_events_without_polling() {
    let (send, receive) = std::sync::mpsc::channel();
    let test = std::thread::spawn(move || {
        let repo = fixture();
        let mut helper = Helper::start();
        assert_eq!(helper.ok(json!({"Hello": 7})), 7);
        helper.ok(json!({"Open": repo.path().to_string_lossy()}));
        helper.ok(json!("WatchRepository"));
        std::fs::rename(repo.path().join("a.txt"), repo.path().join("renamed.txt")).unwrap();
        git(repo.path(), &["add", "-A"]);
        let mut paths = std::collections::BTreeSet::new();
        let mut index = false;
        while !index || !paths.contains("renamed.txt") || !paths.contains("a.txt") {
            let event = helper.read_message();
            assert_eq!(event["id"], 0, "{event}");
            let change = &event["result"]["Ok"];
            index |= change["index"] == true;
            paths.extend(
                change["paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|path| path.as_str().unwrap().to_owned()),
            );
        }
        let versions = helper.ok(json!({"FileVersions": ["renamed.txt"]}));
        assert_eq!(versions[0]["status"]["staged"], true);
        git(repo.path(), &["commit", "-qm", "External commit"]);
        loop {
            let event = helper.read_message();
            assert_eq!(event["id"], 0, "{event}");
            if event["result"]["Ok"]["refs"] == true {
                break;
            }
        }
        send.send(()).unwrap();
    });
    receive
        .recv_timeout(std::time::Duration::from_secs(8))
        .unwrap();
    test.join().unwrap();
}

#[test]
fn workspace_locator_searches_ignored_files_and_resumes_with_the_same_scope() {
    use std::fs;

    let repo = fixture();
    let root = repo.path();
    fs::write(root.join(".gitignore"), "build/\n").unwrap();
    fs::create_dir(root.join("build")).unwrap();
    fs::write(root.join("build/guide.MDX"), "locator_marker\n").unwrap();
    fs::write(root.join("guide.MDX.rs"), "locator_marker\n".repeat(501)).unwrap();
    git(root, &["add", "guide.MDX.rs"]);
    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 30})), 30);
    assert_eq!(helper.ok(json!({"Hello": 31})), 31);
    helper.ok(json!({"OpenWorkspace": root}));
    let query = json!({
        "query": "locator_marker",
        "scope": { "include": "", "exclude": "" },
        "case_sensitive": false, "whole_word": false, "regex": false
    });
    let legacy = helper.ok(json!({"Locator": query}));
    assert_eq!(legacy["items"].as_array().unwrap().len(), 500);
    assert!(
        legacy["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["path"] == "guide.MDX.rs")
    );

    let files = helper.ok(json!({"SearchWorkspaceFiles": {
        "id": 1, "revision": 0, "query": {
            "query": "guide.MDX", "scope": { "include": "*.{MDX,md}", "exclude": "./docs" },
            "recent": [], "file_type": "Markdown"
        }
    }}));
    assert_eq!(files["items"], json!(["build/guide.MDX"]));
    let docs = helper.ok(json!({"SearchWorkspaceText": {
        "id": 2, "revision": 0, "file_type": "Markdown", "query": query
    }}));
    assert_eq!(docs["truncated"], false);
    assert_eq!(docs["items"].as_array().unwrap().len(), 1);
    assert_eq!(docs["items"][0]["path"], "build/guide.MDX");

    let first = helper.ok(json!({"SearchWorkspaceText": {
        "id": 3, "revision": 0, "file_type": "All", "query": query
    }}));
    assert_eq!(first["truncated"], true);
    assert_eq!(first["items"][499]["line"], 499);
    let next = helper.ok(json!({"ContinueWorkspaceSearch": 3}));
    assert_eq!(next["truncated"], false);
    assert_eq!(next["items"][0]["line"], 500);
    assert_eq!(next["items"][1]["path"], "build/guide.MDX");
    helper.ok(json!({"CancelWorkspaceSearch": 3}));
    assert!(
        helper
            .call(json!({"ContinueWorkspaceSearch": 3}))
            .get("Err")
            .is_some()
    );
}

#[test]
fn document_paths_include_ignored_and_nested_repository_markdown() {
    use std::fs;

    let repo = fixture();
    let root = repo.path();
    fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    fs::create_dir_all(root.join("ignored/nested/.git")).unwrap();
    for path in [
        "README.md",
        ".hidden.MDX",
        "ignored/nested/guide.markdown",
        "ignored/nested/.git/internal.md",
        "ignored/nested/file.txt",
    ] {
        fs::write(root.join(path), "").unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("README.md", root.join("link.md")).unwrap();

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 17})), 17);
    helper.ok(json!({"OpenWorkspace": root}));
    let paths = helper.ok(json!("DocumentPaths"));
    let mut paths: Vec<_> = paths
        .as_array()
        .unwrap()
        .iter()
        .map(|path| Path::new(path.as_str().unwrap()).to_owned())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            Path::new(".hidden.MDX"),
            Path::new("README.md"),
            Path::new("ignored/nested/guide.markdown")
        ]
    );
}

#[test]
fn document_batches_start_with_git_paths_and_include_ignored_documents() {
    use std::fs;

    let repo = fixture();
    let root = repo.path();
    fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    fs::create_dir_all(root.join("ignored/nested/.git")).unwrap();
    fs::write(root.join("visible.md"), "").unwrap();
    fs::write(root.join("ignored/Tracked.MDX"), "").unwrap();
    git(root, &["add", "-f", "ignored/Tracked.MDX"]);
    for directory in ["ignored/nested", "ignored/other"] {
        fs::create_dir_all(root.join(directory)).unwrap();
        for index in 0..520 {
            fs::write(root.join(directory).join(format!("doc-{index}.md")), "").unwrap();
        }
    }

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 18})), 18);
    helper.ok(json!({"OpenWorkspace": root}));
    let fast = helper.ok(json!("FastDocumentPaths"));
    let paths: std::collections::BTreeSet<_> = fast
        .as_array()
        .unwrap()
        .iter()
        .map(|value| Path::new(value.as_str().unwrap()))
        .collect();
    assert_eq!(
        paths,
        [Path::new("visible.md"), Path::new("ignored/Tracked.MDX")].into()
    );

    let first = helper.ok(json!({"DocumentBatch": 1}));
    assert_eq!(first["done"], false);
    let last = helper.ok(json!({"DocumentBatch": 1}));
    assert_eq!(last["done"], true);
    assert_eq!(
        first["paths"].as_array().unwrap().len() + last["paths"].as_array().unwrap().len(),
        1_042
    );
    let restarted = helper.ok(json!({"DocumentBatch": 2}));
    assert_eq!(restarted["done"], false);
    assert!(
        helper
            .call(json!({"DocumentBatch": 1}))
            .get("Err")
            .is_some()
    );
}

#[test]
fn workspace_file_protocol_lists_mutates_and_reads_large_files() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::fs;

    let repo = fixture();
    let root = repo.path();
    fs::write(root.join(".gitignore"), "ignored/\n*.log\n!keep.log\n").unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::create_dir(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/notes.txt"), "ignored notes").unwrap();
    for file in [".visible", "keep.log", "tracked.log"] {
        fs::write(root.join(file), "").unwrap();
    }
    git(root, &["add", "-f", "tracked.log"]);

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 8})), json!(8));
    {
        let mut lazy = Helper::start();
        assert_eq!(lazy.ok(json!({"Hello": 16})), json!(16));
        let opened = lazy.ok(json!({"OpenWorkspace": root}));
        assert_eq!(opened["children"].as_object().unwrap().len(), 1);
        assert!(opened["children"][""].is_array());
        let children = lazy.ok(json!({"TreeDirectories": ["ignored"]}));
        assert_eq!(children.as_object().unwrap().len(), 1);
        assert_eq!(children["ignored"][0]["ignored"], true);
    }
    let open = helper.ok(json!({"Open":root}));
    let children = open["children"][""].as_array().unwrap();
    assert!(
        children
            .iter()
            .any(|entry| entry["path"] == "empty" && entry["directory"] == true)
    );
    assert!(children.iter().any(|entry| {
        entry["path"] == "ignored" && entry["directory"] == true && entry["ignored"] == true
    }));
    for path in [".gitignore", ".visible", "keep.log", "tracked.log"] {
        assert!(
            children
                .iter()
                .any(|entry| { entry["path"] == path && entry.get("ignored").is_none() })
        );
    }
    assert!(!children.iter().any(|entry| entry["path"] == ".git"));
    let ignored_path = Path::new("ignored").join("notes.txt");
    let ignored_entry = &open["children"]["ignored"][0];
    assert_eq!(
        Path::new(ignored_entry["path"].as_str().unwrap()),
        ignored_path
    );
    assert_eq!(ignored_entry["ignored"], true);
    let listing = helper.ok(json!({"TreeDirectories": ["", "ignored"]}));
    assert_eq!(listing[""], open["children"][""]);
    assert_eq!(listing["ignored"], open["children"]["ignored"]);
    helper.ok(json!({"FileOperation": ["empty/new.txt", {"Create": {"directory": false}}]}));
    let directory = helper.ok(json!({"TreeDirectories": ["empty"]}));
    let name = directory["empty"][0]["path"].as_str().unwrap();
    assert_eq!(Path::new(name), Path::new("empty").join("new.txt"));

    let token = helper.ok(json!({"EntryVersion": "empty/new.txt"}));
    assert!(helper.call(json!({"FileOperation": ["empty/new.txt", {"Rename": {"name": ".git", "version": token}}]})).get("Err").is_some());
    helper.ok(json!({"FileOperation": ["empty/new.txt", {"Rename": {"name": "renamed.txt", "version": token}}]}));
    assert_eq!(
        helper.ok(json!({"ReadForSync": "empty/new.txt"})),
        Value::Null
    );
    assert_eq!(
        helper.ok(json!("WorktreeStatus"))["empty/renamed.txt"]["untracked"],
        true
    );

    let bytes: Vec<_> = (0..9 * 1024 * 1024 + 37)
        .map(|offset| (offset % 251) as u8)
        .collect();
    fs::write(root.join("empty/renamed.txt"), &bytes).unwrap();
    let current = helper.ok(json!({"EntryVersion": "empty/renamed.txt"}));
    assert!(
        helper
            .call(json!({"FileOperation": ["empty/renamed.txt", {"Delete": {"version": token}}]}))
            .get("Err")
            .is_some()
    );
    for (index, expected) in bytes.chunks(1024 * 1024).enumerate() {
        let chunk = helper
            .ok(json!({"ReadCopyChunk": ["empty/renamed.txt", current, index * 1024 * 1024]}));
        assert_eq!(STANDARD.decode(chunk.as_str().unwrap()).unwrap(), expected);
    }
    let after = helper.ok(json!({"EntryVersion": "empty"}));
    helper.ok(json!({"FileOperation": ["empty", {"Delete": {"version": after}}]}));
    assert!(!root.join("empty").exists());
}

#[test]
fn workspace_file_protocol_lists_nested_repository_contents() {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use stroll::repository::Entry;

    let repo = fixture();
    let root = repo.path();
    let nested = root.join("nested");
    fs::create_dir_all(nested.join("assets")).unwrap();
    fs::write(nested.join("assets/hero.png"), b"png").unwrap();
    fs::write(nested.join("notes.md"), "inner\n").unwrap();
    // An untracked nested repository is reported as one directory entry by Git.
    git(&nested, &["init", "-q", "-b", "main"]);
    // The parent rule matches the nested file, but it does not describe that repository.
    fs::write(root.join(".gitignore"), "*.png\n").unwrap();

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 8})), json!(8));
    let open = helper.ok(json!({"Open":root}));
    let children: BTreeMap<PathBuf, Vec<Entry>> =
        serde_json::from_value(open["children"].clone()).unwrap();
    let top_level = &children[Path::new("")];
    assert!(
        top_level
            .iter()
            .any(|entry| entry.path == Path::new("nested") && entry.directory),
        "{top_level:?}"
    );

    let nested_children = &children[Path::new("nested")];
    assert!(
        nested_children
            .iter()
            .any(|entry| entry.path == Path::new("nested/assets") && entry.directory),
        "{nested_children:?}"
    );
    assert!(
        nested_children
            .iter()
            .any(|entry| entry.path == Path::new("nested/notes.md") && !entry.directory),
        "{nested_children:?}"
    );
    assert!(
        nested_children
            .iter()
            .all(|entry| !entry.path.to_string_lossy().contains(".git"))
    );

    // Files below the nested repository root are listed from disk, not from Git.
    assert!(
        children[Path::new("nested/assets")]
            .iter()
            .any(|entry| entry.path == Path::new("nested/assets/hero.png")),
        "{open}"
    );

    // Expanding the folder also lists its contents instead of returning an empty directory.
    let listing: BTreeMap<PathBuf, Vec<Entry>> =
        serde_json::from_value(helper.ok(json!({"TreeDirectories": ["nested", "nested/assets"]})))
            .unwrap();
    assert!(
        listing[Path::new("nested")]
            .iter()
            .any(|entry| entry.path == Path::new("nested/notes.md")),
        "{listing:?}"
    );
    assert!(
        listing[Path::new("nested/assets")]
            .iter()
            .any(|entry| entry.path == Path::new("nested/assets/hero.png")),
        "{listing:?}"
    );
}

#[test]
fn helper_streams_nested_repository_events_past_parent_ignore_rules() {
    let (send, receive) = std::sync::mpsc::channel();
    let test = std::thread::spawn(move || {
        use std::fs;

        let repo = fixture();
        let root = repo.path();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("hero.png"), b"png").unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        // The parent ignore rule matches the nested file path, but it does not describe
        // that repository's worktree.
        fs::write(root.join(".gitignore"), "*.png\n").unwrap();

        let mut helper = Helper::start();
        assert_eq!(helper.ok(json!({"Hello": 7})), 7);
        helper.ok(json!({"Open": root.to_string_lossy()}));
        helper.ok(json!("WatchRepository"));
        fs::write(nested.join("hero.png"), b"updated").unwrap();
        loop {
            let event = helper.read_message();
            assert_eq!(event["id"], 0, "{event}");
            let changes = &event["result"]["Ok"];
            if changes["paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == "nested/hero.png")
            {
                break;
            }
        }
        send.send(()).unwrap();
    });
    receive
        .recv_timeout(std::time::Duration::from_secs(8))
        .unwrap();
    test.join().unwrap();
}

#[test]
fn helper_commit_and_undo_keep_the_index_and_worktree_separate() {
    let repo = fixture();
    let root = repo.path();
    let added = "new file.bin";
    let deleted = "b with space.txt";
    std::fs::write(root.join(added), [0, 0xff, 3]).unwrap();
    std::fs::remove_file(root.join(deleted)).unwrap();
    let connect = || {
        let mut helper = Helper::start();
        assert_eq!(helper.ok(json!({ "Hello": 11 })), 11);
        helper.ok(json!({ "OpenWorkspace": root.to_string_lossy() }));
        helper
    };
    let mut helper = connect();
    let expected = helper.ok(json!("WorktreeStatus"));
    let staged = helper.ok(json!({"StageWithHistory": [expected, [added, deleted], true]}));
    let history = helper.ok(json!("OperationHistory"));
    assert_eq!(staged["id"], history[0]["id"]);
    assert_eq!(history[0]["paths"], json!([deleted, added]));
    let after_stage = helper.ok(json!("WorktreeStatus"));
    assert_eq!(after_stage[added]["staged"], true);
    assert_eq!(after_stage[deleted]["staged"], true);

    helper.ok(json!({"UndoOperation": history[0]["id"]}));
    assert!(!root.join(deleted).exists());
    assert_eq!(std::fs::read(root.join(added)).unwrap(), [0, 0xff, 3]);
    helper.ok(json!({"ReplayGitHistory": [staged["id"], false]}));
    assert_eq!(helper.ok(json!("WorktreeStatus"))[added]["staged"], true);
    assert!(!root.join(deleted).exists());
    helper.ok(json!({"ReplayGitHistory": [staged["id"], true]}));
    for path in [added, deleted] {
        let version = helper.ok(json!({"FileVersion": path}));
        helper.ok(json!({"EditOperation": {
            "path": path, "version": version, "action": "Restore", "block": null
        }}));
    }
    assert!(!root.join(added).exists());
    assert_eq!(
        std::fs::read_to_string(root.join(deleted)).unwrap(),
        "alpha\nbeta\n"
    );

    drop(helper);
    let mut helper = connect();
    let history = helper.ok(json!("OperationHistory"));
    assert_eq!(history.as_array().unwrap().len(), 3);
    assert_eq!(history[2]["availability"], "Undone");
    helper.ok(json!({"UndoOperation": history[0]["id"]}));
    assert!(!root.join(deleted).exists());
    helper.ok(json!({"ReplayGitHistory": [history[0]["id"], false]}));
    assert_eq!(
        std::fs::read_to_string(root.join(deleted)).unwrap(),
        "alpha\nbeta\n"
    );
    helper.ok(json!({"ReplayGitHistory": [history[0]["id"], true]}));
    let version = helper.ok(json!({"FileVersion": deleted}));
    helper.ok(json!({"EditOperation": {
        "path": deleted, "version": version, "action": "Restore", "block": null
    }}));
    let restored_deletion = helper.ok(json!("OperationHistory"))[0]["id"].clone();
    std::fs::write(root.join(deleted), "edited externally\n").unwrap();
    assert!(
        helper
            .call(json!({"UndoOperation": restored_deletion}))
            .get("Err")
            .is_some()
    );
    assert_eq!(
        std::fs::read_to_string(root.join(deleted)).unwrap(),
        "edited externally\n"
    );
    helper.ok(json!({"UndoOperation": history[1]["id"]}));
    assert_eq!(std::fs::read(root.join(added)).unwrap(), [0, 0xff, 3]);

    let expected = helper.ok(json!("WorktreeStatus"));
    helper.ok(json!({"StageOperation": [expected, ["a.txt"], true]}));
    let commit = helper.ok(json!("CommitState"));
    assert_eq!(commit["ready"], true);
    std::fs::write(root.join("a.txt"), "change after stage\n").unwrap();
    git(root, &["add", "--", deleted]);
    assert!(
        helper
            .call(json!({"CreateCommit": [commit, "Stale message"]}))
            .get("Err")
            .is_some()
    );
    git(root, &["restore", "--staged", "--", deleted]);
    let commit = helper.ok(json!("CommitState"));
    helper.ok(json!({"CreateCommit": [commit, "Commit subject\n\nCommit body"]}));
    let history = helper.ok(json!("OperationHistory"));
    assert_eq!(history[0]["availability"], "Unavailable");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", "HEAD:a.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"one\nTWO\nthree\n");
    assert_eq!(
        std::fs::read(root.join("a.txt")).unwrap(),
        b"change after stage\n"
    );
}

#[test]
fn helper_stage_undo_preserves_intent_to_add_even_if_the_file_disappeared() {
    let repo = fixture();
    let file = "intent.txt";
    let root = repo.path();
    std::fs::write(root.join(file), "").unwrap();
    git(root, &["add", "-N", "--", file]);
    let mut helper = Helper::start();
    helper.ok(json!({ "OpenWorkspace": root.to_string_lossy() }));
    let expected = helper.ok(json!("WorktreeStatus"));
    helper.ok(json!({"StageOperation": [expected, [file], true]}));
    assert_eq!(helper.ok(json!("WorktreeStatus"))[file]["staged"], true);
    std::fs::remove_file(root.join(file)).unwrap();
    let history = helper.ok(json!("OperationHistory"));
    helper.ok(json!({"UndoOperation": history[0]["id"]}));
    assert!(!root.join(file).exists());
    assert_eq!(helper.ok(json!("WorktreeStatus"))[file]["staged"], false);
}

#[test]
fn helper_blame_reads_the_committed_file_instead_of_the_worktree() {
    let repo = fixture();
    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({ "Hello": 12 })), json!(12));
    helper.ok(json!({ "OpenWorkspace": repo.path().to_string_lossy() }));
    let version = helper.ok(json!({ "FileVersion": "b with space.txt" }));
    let head = version["head"].as_str().unwrap();
    assert_eq!(
        helper.ok(json!({ "BlameBaseline": ["b with space.txt", head] })),
        json!("alpha\nbeta\n")
    );
    let lines = helper.ok(json!({ "Blame": ["b with space.txt", head, 2, 2] }));
    assert_eq!(lines[0]["oid"], head);
    assert_eq!(lines[0]["line"], 2);
    assert_eq!(lines[0]["author"], "Test");
    assert_eq!(lines[0]["subject"], "initial");
}

#[test]
fn helper_supplies_staged_ai_context_and_validates_hover_paths() {
    let repo = fixture();
    let root = repo.path();
    git(root, &["update-index", "--split-index"]);
    git(root, &["add", "a.txt"]);
    std::fs::write(root.join("a.txt"), "one\nunstaged-marker\nthree\n").unwrap();
    std::fs::write(root.join(".env"), "private-marker").unwrap();
    std::fs::write(root.join("binary"), b"\x00\x01binary-marker").unwrap();
    git(root, &["add", ".env", "binary"]);

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({ "Hello": 14 })), 14);
    helper.ok(json!({ "Open": root.to_string_lossy() }));
    assert_eq!(
        helper.ok(json!({ "AllowsHoverTranslation": "a.txt" })),
        true
    );
    assert_eq!(
        helper.ok(json!({ "AllowsHoverTranslation": ".env" })),
        false
    );
    assert_eq!(
        helper.ok(json!({ "AllowsHoverTranslation": "../outside.txt" })),
        false
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(".env", root.join("renamed.txt")).unwrap();
        assert_eq!(
            helper.ok(json!({ "AllowsHoverTranslation": "renamed.txt" })),
            false
        );
    }
    let state = helper.ok(json!("CommitState"));
    let context = helper.ok(json!({ "StagedCommitContext": state.clone() }));
    assert!(context["patch"].as_str().unwrap().contains("+TWO"));
    assert!(context["patch"].as_str().unwrap().contains("Binary files"));
    assert_eq!(context["omitted"], 1);
    let context = context.to_string();
    for excluded in [
        "unstaged-marker",
        "b with space.txt",
        ".env",
        "private-marker",
        "binary-marker",
    ] {
        assert!(!context.contains(excluded), "{excluded}");
    }
    git(root, &["add", "a.txt"]);
    assert!(
        helper
            .call(json!({ "StagedCommitContext": state }))
            .get("Err")
            .is_some()
    );
}

#[test]
fn image_diff_versions_agree_with_disk_and_git_objects() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let repo = fixture();
    let root = repo.path();
    let image = root.join("picture.ppm");
    let bytes = |pixel| [b"P6\n1 1\n255\n".as_slice(), &[pixel, 0, 0]].concat();
    std::fs::write(&image, bytes(1)).unwrap();
    git(root, &["add", "picture.ppm"]);
    git(root, &["commit", "-qm", "Add picture"]);
    std::fs::write(&image, bytes(2)).unwrap();

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({ "Hello": 23 })), 23);
    helper.ok(json!({ "OpenWorkspace": root }));
    let detail = helper.ok(json!({ "ReviewFiles": comparison() }));
    let base = &detail["base"];
    let request = |side, version: &Value| json!({ "ImageDiffResource": [comparison(), base, "picture.ppm", side, version] });
    let stored = helper.ok(request("Old", &Value::Null));
    let oid = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD:picture.ppm"])
        .output()
        .unwrap();
    assert!(oid.status.success());
    assert_eq!(
        stored["Version"]["Blob"],
        String::from_utf8_lossy(&oid.stdout).trim()
    );
    assert_eq!(
        helper.ok(request("Old", &stored["Version"]))["Content"],
        STANDARD.encode(bytes(1))
    );

    let changed = helper.ok(request("New", &Value::Null));
    let disk = &changed["Version"];
    let check = |version: &Value| {
        json!({ "ReadResource": {
            "path": "picture.ppm", "asset": true, "version": version["Worktree"]
        } })
    };
    assert_eq!(helper.ok(check(disk)), "Unchanged");
    assert_eq!(
        helper.ok(request("New", disk))["Content"],
        STANDARD.encode(bytes(2))
    );
    std::fs::write(&image, bytes(3)).unwrap();
    assert!(helper.call(request("New", disk)).get("Err").is_some());
    let latest = helper.ok(request("New", &Value::Null));
    assert_ne!(latest["Version"], *disk);
    assert_eq!(
        helper.ok(request("New", &latest["Version"]))["Content"],
        STANDARD.encode(bytes(3))
    );
    assert_eq!(
        helper.ok(json!({ "ImageDiffSide": [comparison(), base, "picture.ppm", "New"] })),
        STANDARD.encode(bytes(3))
    );
}

#[test]
fn helper_manages_worktrees_and_branches_independently() {
    let repo = fixture();
    let root = repo.path();
    let checkouts = tempfile::tempdir().unwrap();
    let linked = checkouts.path().join("linked checkout");
    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 32})), 32);
    assert_eq!(helper.ok(json!({"Hello": 33})), 33);
    helper.ok(json!({"OpenWorkspace": root}));

    helper.ok(json!({"ManageGit": {"AddTree": {
        "path": linked, "branch": "topic", "start": "HEAD"
    }}}));
    let state = helper.ok(json!("GitManagement"));
    let topic = state["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["branch"]["label"] == "topic")
        .unwrap();
    let oid = topic["branch"]["oid"].clone();
    assert_eq!(topic["worktree"], json!(linked));
    let tree = state["trees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["tree"]["reference"] == "topic")
        .unwrap();
    assert_eq!(tree["clean"], true);

    assert!(
        helper
            .call(json!({"ManageGit": {"DeleteBranch": {
                "name": "topic", "oid": oid
            }}}))
            .get("Err")
            .is_some()
    );
    let remove = json!({"ManageGit": {"RemoveTree": {"path": linked, "reference": "topic"}}});
    helper.ok(json!({"ManageGit": {"LockTree": {"path": linked, "locked": true}}}));
    assert!(helper.call(remove.clone()).get("Err").is_some());
    helper.ok(json!({"ManageGit": {"LockTree": {"path": linked, "locked": false}}}));
    let ignored = linked.join("ignored.tmp");
    git(
        &linked,
        &[
            "config",
            "core.excludesFile",
            checkouts.path().join("git-ignore").to_str().unwrap(),
        ],
    );
    std::fs::write(checkouts.path().join("git-ignore"), "ignored.tmp\n").unwrap();
    std::fs::write(&ignored, "Keep this file").unwrap();
    assert!(helper.call(remove.clone()).get("Err").is_some());
    assert!(ignored.is_file());
    let delete = |mode| {
        json!({"ManageGit": {"RemoveTree": {
            "path": linked, "reference": "topic", "mode": mode
        }}})
    };
    assert!(helper.call(delete("MissingDirectory")).get("Err").is_some());
    assert!(ignored.is_file());

    helper.ok(delete("Discard"));
    assert!(!linked.exists());
    git(root, &["show-ref", "--verify", "refs/heads/topic"]);
    helper.ok(json!({"ManageGit": {"RenameBranch": {"name": "topic", "new_name": "archived", "oid": oid}}}));
    helper.ok(json!({"ManageGit": {"DeleteBranch": {"name": "archived", "oid": oid}}}));
    let state = helper.ok(json!("GitManagement"));
    assert!(
        !state["branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["branch"]["label"] == "archived")
    );

    git(
        root,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    git(
        &linked,
        &["commit", "--allow-empty", "-qm", "detached work"],
    );
    let state = helper.ok(json!("GitManagement"));
    let detached = state["trees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["tree"]["root"] == json!(linked))
        .unwrap();
    assert_eq!(detached["detached_unreferenced"], true);
    let remove = json!({"ManageGit": {"RemoveTree": {
        "path": linked, "reference": detached["tree"]["reference"]
    }}});
    assert!(helper.call(remove.clone()).get("Err").is_some());
    git(&linked, &["branch", "keep-detached-commit"]);
    helper.ok(remove);
    git(
        root,
        &["show-ref", "--verify", "refs/heads/keep-detached-commit"],
    );

    let stale = checkouts.path().join("stale checkout");
    let moved = checkouts.path().join("moved checkout");
    git(
        root,
        &["worktree", "add", "-qb", "stale", stale.to_str().unwrap()],
    );
    git(
        root,
        &["worktree", "add", "-qb", "other", linked.to_str().unwrap()],
    );
    std::fs::rename(&stale, &moved).unwrap();
    let other = checkouts.path().join("other checkout moved");
    std::fs::rename(&linked, other).unwrap();
    let state = helper.ok(json!("GitManagement"));
    assert!(state["trees"].as_array().unwrap().iter().any(|entry| {
        entry["tree"]["root"] == json!(stale) && entry["tree"]["missing"] == true
    }));
    helper.ok(json!({"ManageGit": {"RemoveTree": {
        "path": stale, "reference": "stale", "mode": "MissingDirectory"
    }}}));
    let state = helper.ok(json!("GitManagement"));
    let trees = state["trees"].as_array().unwrap();
    assert!(
        !trees
            .iter()
            .any(|entry| entry["tree"]["root"] == json!(stale))
    );
    assert!(
        trees
            .iter()
            .any(|entry| entry["tree"]["root"] == json!(linked))
    );
    assert!(moved.is_dir());
    git(root, &["show-ref", "--verify", "refs/heads/stale"]);
}

#[test]
fn helper_reviews_and_deletes_only_the_selected_stale_worktree() {
    let repo = fixture();
    let root = repo.path();
    let checkouts = tempfile::tempdir().unwrap();
    let stale = checkouts.path().join("stale");
    let other = checkouts.path().join("other");
    for path in [&stale, &other] {
        git(
            root,
            &["worktree", "add", "--detach", path.to_str().unwrap()],
        );
    }
    git(&stale, &["commit", "--allow-empty", "-qm", "Detached work"]);
    for path in [&stale, &other] {
        std::fs::remove_file(path.join(".git")).unwrap();
        std::fs::write(path.join("local.txt"), "keep").unwrap();
    }
    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 37})), 37);
    helper.ok(json!({"OpenWorkspace": root}));
    let management = helper.ok(json!("GitManagement"));
    let tree = management["trees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["tree"]["root"] == json!(stale))
        .unwrap();
    let preview_request = json!({"PreviewWorktreeRemoval": [stale, tree["tree"]["reference"]]});
    let preview = helper.ok(preview_request.clone());
    assert_eq!(preview["detached_unreferenced"], true);
    assert_eq!(
        preview["entries"],
        json!([["", "a.txt"], ["", "b with space.txt"], ["", "local.txt"]])
    );
    assert!(preview["action"]["DeleteStaleDirectory"]["reason"].is_string());
    let saved = checkouts.path().join("stale.stroll-saved");
    std::fs::create_dir(&saved).unwrap();
    std::fs::write(saved.join("local.txt"), "previously saved").unwrap();

    std::fs::write(stale.join("new.txt"), "created after preview").unwrap();
    let confirm = |preview| json!({"ManageGit": {"RemoveReviewedTree": preview}});
    assert!(helper.call(confirm(preview)).get("Err").is_some());
    assert!(stale.join("local.txt").is_file());

    let preview = helper.ok(preview_request);
    assert_eq!(preview["entry_count"], 4);
    helper.ok(confirm(preview));
    assert!(!stale.exists());
    assert!(other.join("local.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(saved.join("local.txt")).unwrap(),
        "previously saved"
    );
    assert!(!checkouts.path().join("stale.stroll-saved-2").exists());
    let management = helper.ok(json!("GitManagement"));
    let trees = management["trees"].as_array().unwrap();
    assert!(
        !trees
            .iter()
            .any(|entry| entry["tree"]["root"] == json!(stale))
    );
    assert!(
        trees
            .iter()
            .any(|entry| entry["tree"]["root"] == json!(other))
    );
}

#[test]
fn helper_fetches_publishes_and_fast_forwards_an_explicit_upstream() {
    let repo = fixture();
    let root = repo.path();
    git(root, &["restore", "."]);
    let host = tempfile::tempdir().unwrap();
    let bare = host.path().join("remote.git");
    git(
        host.path(),
        &[
            "init",
            "--bare",
            "-qb",
            "integration",
            bare.to_str().unwrap(),
        ],
    );

    let mut helper = Helper::start();
    helper.ok(json!({"OpenWorkspace": root}));
    helper.ok(json!({"ManageGit": {"AddRemote": {"name": "origin", "url": bare}}}));
    let before = helper.ok(json!("History"));
    helper.ok(json!({"ManageGit": {"Push": {
        "source": "refs/heads/main", "oid": before["head"], "remote": "origin",
        "branch": "integration", "set_upstream": true
    }}}));
    let tracked = helper.ok(json!("History"));
    let main = tracked["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["label"] == "main")
        .unwrap();
    assert_eq!(
        main["tracking"]["reference"],
        "refs/remotes/origin/integration"
    );

    git(root, &["commit", "--allow-empty", "-qm", "push normally"]);
    let before = helper.ok(json!("History"));
    helper.ok(json!({"ManageGit": {"Push": {
        "source": "refs/heads/main", "oid": before["head"], "remote": "origin",
        "branch": "integration", "set_upstream": false
    }}}));

    let peer = host.path().join("peer");
    git(
        host.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            peer.to_str().unwrap(),
        ],
    );
    git(
        &peer,
        &[
            "-c",
            "user.name=Peer",
            "-c",
            "user.email=peer@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "upstream",
        ],
    );
    git(&peer, &["push", "-q"]);
    helper.ok(json!({"ManageGit": {"Fetch": {"remote": "origin"}}}));
    let fetched = helper.ok(json!("History"));
    assert_eq!(fetched["head"], before["head"]);
    let main = fetched["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["label"] == "main")
        .unwrap();
    assert_eq!(main["tracking"]["behind"], 1);

    let pull = |oid: &Value| {
        json!({"ManageGit": {"Pull": {
            "source": "refs/heads/main", "oid": oid, "remote": "origin", "upstream": "refs/remotes/origin/integration"
        }}})
    };
    helper.ok(pull(&before["head"]));
    let pulled = helper.ok(json!("History"));
    assert_ne!(pulled["head"], before["head"]);

    git(
        &peer,
        &[
            "-c",
            "user.name=Peer",
            "-c",
            "user.email=peer@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "more upstream",
        ],
    );
    git(&peer, &["push", "-q"]);
    git(root, &["commit", "--allow-empty", "-qm", "local only"]);
    let diverged = helper.ok(json!("History"));
    assert!(helper.call(pull(&diverged["head"])).get("Err").is_some());
    let still_diverged = helper.ok(json!("History"));
    assert_eq!(still_diverged["head"], diverged["head"]);
    let main = still_diverged["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|branch| branch["label"] == "main")
        .unwrap();
    assert_eq!(main["tracking"]["ahead"], 1);
    assert_eq!(main["tracking"]["behind"], 1);
    let comparison = helper
        .ok(json!({"CompareUpstream": ["refs/heads/main", "refs/remotes/origin/integration"]}));
    assert_eq!(comparison["outgoing"][0]["subject"], "local only");
    assert_eq!(comparison["incoming"][0]["subject"], "more upstream");
}

#[test]
fn helper_reviews_the_live_push_target_and_requires_an_exact_lease() {
    let repo = fixture();
    let root = repo.path();
    let temp = tempfile::tempdir().unwrap();
    let bare = |name| {
        let path = temp.path().join(name);
        std::fs::create_dir(&path).unwrap();
        git(&path, &["init", "--bare", "-qb", "main"]);
        path
    };
    let fetch = bare("fetch");
    let publish = bare("publish");
    let publish_url = publish.to_str().unwrap();
    git(root, &["push", publish_url, "main"]);
    git(root, &["remote", "add", "origin", fetch.to_str().unwrap()]);
    git(
        root,
        &["remote", "set-url", "--push", "origin", publish_url],
    );
    git(root, &["remote", "add", "secondary", publish_url]);
    git(root, &["config", "branch.main.pushRemote", "secondary"]);

    let peer = temp.path().join("peer");
    git(root, &["clone", "-q", publish_url, peer.to_str().unwrap()]);
    git(&peer, &["config", "user.name", "Peer"]);
    git(&peer, &["config", "user.email", "peer@example.invalid"]);
    git(&peer, &["config", "commit.gpgsign", "false"]);
    git(root, &["commit", "-qm", "local change", "--", "a.txt"]);

    let mut helper = Helper::start();
    assert_eq!(helper.ok(json!({"Hello": 34})), 34);
    assert_eq!(helper.ok(json!({"Hello": 35})), 35);
    helper.ok(json!({"OpenWorkspace": root}));
    assert_eq!(
        helper.ok(json!({"PushBranches": "origin"})),
        json!(["main"])
    );
    let history = helper.ok(json!("History"));
    let options = helper.ok(json!({"PushOptions": history["checkout"]}));
    assert_eq!(options["remote"], "secondary");
    assert_eq!(options["remotes"], json!(["origin", "secondary"]));
    assert_eq!(options["set_upstream"], true);
    let target = |branch| {
        json!({
            "source": history["checkout"], "oid": history["head"], "remote": "origin", "branch": branch,
        })
    };
    let main = target("main");
    let alternate = target("review/alternate");
    let preview = |helper: &mut Helper, target: &Value| helper.ok(json!({"PreviewPush": target}));
    let push = |target: &Value, review: &Value, force, set_upstream| {
        json!({
            "ManageGit": {"PushReviewed": {"target": target, "lease": review["lease"], "force": force, "set_upstream": set_upstream}}
        })
    };

    let creating = preview(&mut helper, &alternate);
    assert!(creating["lease"]["target_oid"].is_null());
    assert_eq!(creating["new_branch_base"], "main");
    assert_eq!(creating["outgoing"].as_array().unwrap().len(), 1);
    assert_eq!(creating["outgoing"][0]["subject"], "local change");
    assert_eq!(creating["files"].as_array().unwrap().len(), 1);
    assert_eq!(creating["files"][0]["path"], "a.txt");
    assert_eq!(creating["files"][0]["counts"], json!([1, 1]));
    assert_eq!(creating["overwritten"], 0);
    helper.ok(push(&alternate, &creating, false, true));
    git(
        &publish,
        &["show-ref", "--verify", "refs/heads/review/alternate"],
    );
    assert_eq!(
        helper.ok(json!({"PushBranches": "origin"})),
        json!(["main", "review/alternate"])
    );
    git(root, &["rev-parse", "--verify", "main@{upstream}"]);
    let initial = preview(&mut helper, &main);
    assert_eq!(initial["overwritten"], 0);
    assert!(!initial["lease"]["target_oid"].is_null());

    std::fs::write(peer.join("from-peer.txt"), "remote change\n").unwrap();
    git(&peer, &["add", "from-peer.txt"]);
    git(&peer, &["commit", "-qm", "remote change"]);
    git(&peer, &["push", "origin", "main"]);
    assert!(
        helper
            .call(push(&main, &initial, true, false))
            .get("Err")
            .is_some()
    );
    assert!(
        helper
            .call(push(&main, &initial, false, false))
            .get("Err")
            .is_some()
    );

    let diverged = preview(&mut helper, &main);
    assert_eq!(diverged["overwritten"], 1);
    assert_eq!(diverged["outgoing"][0]["subject"], "local change");
    assert_eq!(diverged["files"][1]["path"], "from-peer.txt");
    assert_eq!(diverged["files"][1]["code"], "D");
    assert!(!root.join(".git/FETCH_HEAD").exists());
    assert!(
        helper
            .call(push(&main, &diverged, false, false))
            .get("Err")
            .is_some()
    );

    git(
        root,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            fetch.to_str().unwrap(),
        ],
    );
    assert!(
        helper
            .call(push(&main, &diverged, true, false))
            .get("Err")
            .is_some()
    );
    git(
        root,
        &["remote", "set-url", "--push", "origin", publish_url],
    );
    helper.ok(push(&main, &diverged, true, false));
    let up_to_date = preview(&mut helper, &main);
    assert_eq!(up_to_date["lease"]["target_oid"], history["head"]);
    assert_eq!(up_to_date["outgoing"], json!([]));
    assert_eq!(up_to_date["files"], json!([]));

    git(
        root,
        &["commit", "-qm", "second change", "--", "b with space.txt"],
    );
    let newer = helper.ok(json!("History"));
    let mut fast_forward = main.clone();
    fast_forward["oid"] = newer["head"].clone();
    let reviewed = preview(&mut helper, &fast_forward);
    assert_eq!(reviewed["outgoing"][0]["subject"], "second change");
    assert_eq!(reviewed["overwritten"], 0);
    helper.ok(push(&fast_forward, &reviewed, false, false));
    let updated = preview(&mut helper, &fast_forward);
    assert_eq!(updated["lease"]["target_oid"], newer["head"]);
}
