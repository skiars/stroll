CREATE TABLE startup_location (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value BLOB NOT NULL
);
CREATE TABLE recent_repositories (
    common_dir BLOB PRIMARY KEY NOT NULL,
    root BLOB NOT NULL UNIQUE,
    opened INTEGER NOT NULL,
    value BLOB NOT NULL
);
CREATE INDEX recent_repositories_by_opened ON recent_repositories (opened DESC);

CREATE TABLE worktrees (
    id TEXT PRIMARY KEY NOT NULL,
    common_dir BLOB NOT NULL,
    updated INTEGER NOT NULL,
    value BLOB NOT NULL
);
CREATE INDEX worktrees_by_project ON worktrees (common_dir, updated DESC, id DESC);

CREATE TABLE window_geometry (
    slot TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
);
CREATE TABLE terminal_height (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    height REAL NOT NULL
);

CREATE TABLE worktree_session_fields (
    session BLOB NOT NULL,
    field TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (session, field)
);
CREATE TABLE session_positions (
    session BLOB NOT NULL,
    kind TEXT NOT NULL,
    path BLOB NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (session, kind, path)
);
CREATE TABLE standalone_sessions (
    path BLOB PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
);
CREATE TABLE lsp_trust (
    checkout BLOB PRIMARY KEY NOT NULL
);

CREATE TABLE documents (
    key TEXT PRIMARY KEY NOT NULL,
    baseline BLOB NOT NULL,
    buffer BLOB NOT NULL,
    draft TEXT,
    history BLOB,
    undo_group INTEGER,
    redo_group INTEGER
);
CREATE TABLE edit_groups (
    id INTEGER PRIMARY KEY,
    paths BLOB NOT NULL
);
CREATE TABLE recorded_restores (
    worktree BLOB NOT NULL,
    id INTEGER NOT NULL,
    document TEXT NOT NULL,
    path BLOB NOT NULL,
    PRIMARY KEY (worktree, id)
);
