//! End-to-end stdio protocol tests for `stroll-remote`.
//!
//! Send frames to a real helper to cover Windows paths, grouped diffs, and invalidation after staging.

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
        let mut child = Command::new(env!("CARGO_BIN_EXE_stroll-remote"))
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
