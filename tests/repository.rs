use std::{fs, path::Path, process::Command};

use anyhow::Result;
use stroll::{
    repository::{Document, Repository, Store},
    tabs::TabSession,
};

fn fixture() -> Result<(tempfile::TempDir, Repository, Store)> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("repo");
    fs::create_dir(&root)?;
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    fs::write(root.join("test.txt"), "\u{feff}原文\r\n")?;
    let repo = Repository::open(&root)?;
    let store = Store::new(dir.path().join("state"))?;
    Ok((dir, repo, store))
}

#[test]
fn restores_chinese_draft_and_saves_original_encoding() -> Result<()> {
    let (_dir, repo, store) = fixture()?;
    let path = Path::new("test.txt");
    let doc = Document::open(&repo, path, &store)?;
    doc.persist_draft(&repo, path, &store, "中文输入\n", true)?;
    assert_eq!(
        fs::read_to_string(repo.root.join(path))?,
        "\u{feff}原文\r\n"
    );
    let mut doc = Document::open(&repo, path, &store)?;
    assert!(doc.restored);
    assert_eq!(doc.text, "中文输入\n");
    doc.save(&repo, path, &store, "中文输入\n")?;
    assert_eq!(
        fs::read_to_string(repo.root.join(path))?,
        "\u{feff}中文输入\r\n"
    );
    assert!(!Document::open(&repo, path, &store)?.restored);
    Ok(())
}

#[test]
fn opens_current_draft_storage_without_reading_previous_formats() -> Result<()> {
    use sha2::{Digest as _, Sha256};

    let (dir, repo, store) = fixture()?;
    let path = Path::new("test.txt");
    let state = dir.path().join("state");
    let key = hex::encode(Sha256::digest(
        repo.root.join(path).as_os_str().as_encoded_bytes(),
    ));
    let legacy_dir = state.join("drafts");
    fs::create_dir(&legacy_dir)?;
    let legacy_draft = legacy_dir.join(format!("{key}.json"));
    let previous_database = state.join("edit-history-v1.sqlite3");
    fs::write(&legacy_draft, "old draft")?;
    fs::write(&previous_database, "old database")?;

    let document = Document::open(&repo, path, &store)?;
    assert!(!document.restored);
    document.persist_draft(&repo, path, &store, "new draft", true)?;
    let reopened = Store::new(state)?;
    assert_eq!(Document::open(&repo, path, &reopened)?.text, "new draft");
    assert_eq!(fs::read_to_string(legacy_draft)?, "old draft");
    assert_eq!(fs::read_to_string(previous_database)?, "old database");
    Ok(())
}

#[test]
fn preserves_external_edit_and_conflicting_draft() -> Result<()> {
    let (_dir, repo, store) = fixture()?;
    let path = Path::new("test.txt");
    let mut doc = Document::open(&repo, path, &store)?;
    doc.persist_draft(&repo, path, &store, "本机草稿\n", true)?;
    fs::write(repo.root.join(path), "external")?;
    assert!(doc.save(&repo, path, &store, "本机草稿\n").is_err());
    let doc = Document::open(&repo, path, &store)?;
    assert!(doc.conflict);
    assert_eq!(doc.text, "本机草稿\n");
    assert_eq!(fs::read_to_string(repo.root.join(path))?, "external");
    assert!(Document::open(&repo, Path::new("../state/workspace.lock"), &store).is_err());
    Ok(())
}

#[test]
fn standalone_local_tab_restores_and_keeps_its_disk_baseline() -> Result<()> {
    let (dir, repo, store) = fixture()?;
    let path = dir.path().canonicalize()?.join("notes");
    fs::write(&path, "\u{feff}before\r\n")?;
    let mut tabs = TabSession::default();
    tabs.open(path.clone(), false);
    store.save_tabs(&repo, &tabs)?;
    let mut doc = Document::open(&repo, &path, &store)?;
    doc.persist_draft(&repo, &path, &store, "draft\n", true)?;
    drop(store);

    let store = Store::new(dir.path().join("state"))?;
    assert_eq!(store.tabs(&repo)?.active(), Some(path.as_path()));
    assert_eq!(Document::open(&repo, &path, &store)?.text, "draft\n");
    doc.save(&repo, &path, &store, "draft\n")?;
    assert_eq!(fs::read_to_string(&path)?, "\u{feff}draft\r\n");

    doc.persist_draft(&repo, &path, &store, "second draft\n", true)?;
    fs::write(&path, "disk edit")?;
    assert!(doc.save(&repo, &path, &store, "second draft\n").is_err());
    assert_eq!(fs::read_to_string(&path)?, "disk edit");
    let doc = Document::open(&repo, &path, &store)?;
    assert!(doc.conflict);
    assert_eq!(doc.text, "second draft\n");
    Ok(())
}

#[test]
fn drafts_for_different_tabs_can_be_reopened_and_saved_independently() -> Result<()> {
    let (dir, repo, store) = fixture()?;
    let first = Path::new("test.txt");
    let second = Path::new("other.txt");
    fs::write(repo.root.join(second), "second\n")?;
    for (path, draft) in [(first, "第一份草稿\n"), (second, "第二份草稿\n")] {
        Document::open(&repo, path, &store)?.persist_draft(&repo, path, &store, draft, true)?;
    }
    let mut tabs = TabSession::default();
    tabs.open(first.into(), false);
    tabs.open(second.into(), false);
    store.save_tabs(&repo, &tabs)?;
    store.remember_repository(&repo)?;
    drop(store);

    let store = Store::new(dir.path().join("state"))?;
    assert_eq!(store.tabs(&repo)?.active(), Some(second));
    let mut doc = Document::open(&repo, first, &store)?;
    assert_eq!(doc.text, "第一份草稿\n");
    let first_text = doc.text.clone();
    doc.save(&repo, first, &store, &first_text)?;
    assert!(!Document::open(&repo, first, &store)?.restored);
    assert_eq!(Document::open(&repo, second, &store)?.text, "第二份草稿\n");
    assert_eq!(fs::read_to_string(repo.root.join(second))?, "second\n");
    Ok(())
}

/// Run with a disposable SSH checkout containing a tracked UTF-8 text file.
#[test]
#[ignore = "requires a configured SSH host and a disposable remote repository"]
fn ssh_repository_reopens_and_preserves_saved_text() -> Result<()> {
    let root = std::env::var("STROLL_TEST_SSH_ROOT")?;
    let path = std::env::var("STROLL_TEST_SSH_FILE")?;
    let path = Path::new(&path);
    let state = tempfile::tempdir()?;
    let store = Store::new(state.path().to_owned())?;
    let repo = Repository::open(Path::new(&root))?;
    let doc = Document::open(&repo, path, &store)?;
    let initial = doc.saved_text.clone();
    let edited = format!("{initial}\nSSH 中文编辑\n");
    doc.persist_draft(&repo, path, &store, &edited, true)?;
    drop(doc);
    drop(repo);

    let repo = Repository::open(Path::new(&root))?;
    let mut doc = Document::open(&repo, path, &store)?;
    assert!(doc.restored);
    assert_eq!(doc.text, edited);
    doc.save(&repo, path, &store, &edited)?;
    assert_eq!(Document::open(&repo, path, &store)?.saved_text, edited);
    doc.save(&repo, path, &store, &initial)?;
    Ok(())
}
