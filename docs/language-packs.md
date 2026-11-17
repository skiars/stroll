# Language packs

Bash highlighting is built in. It recognizes `.sh`, `.bash`, common Bash
startup files, and extensionless scripts with a Bash shebang. The Markdown
fence names `bash` and `sh` use the same grammar.

## TextMate extensions

Add a `.stroll.json` file to the worktree:

```json
{
  "languageExtensionPaths": ["./editors/example-language"]
}
```

Each directory points to an unpacked extension with a `package.json`.
Stroll reads `contributes.languages` and `contributes.grammars` and
accepts JSON with comments and trailing commas. The grammar file must be
a JSON TextMate grammar inside the extension directory. Exact filenames
win over suffixes, and the longest suffix wins among suffix matches.
Repository resources are scoped to that worktree. Local repositories accept
absolute extension directories. On SSH, put repository extension resources
inside the remote worktree. Reopen the worktree after changing the resources.

TextMate scopes use Stroll's current code theme. The editor can also
read bracket pairs, automatic closing pairs, `autoCloseBefore`, and basic
increase and decrease indentation patterns. The desktop editing rules are
limited to the settings its editor implements. TextMate and the VS Code
extension manifest are declarative, so extension JavaScript, commands,
activation events, and language server declarations are ignored by this
loader. The supported grammar format is JSON without grammar injections.

## Installing a TextMate language pack

Put the unpacked extension directory under `~/.stroll/language-packs/`.
Each pack uses the same `package.json` and JSON TextMate grammar format as
worktree extensions. `STROLL_HOME` can point to a different application
directory. Reopen a worktree or restart the app for standalone files to pick
up new or updated packs.

## Connecting a language server

Register a server separately in the application-owned
`~/.stroll/lsp.json`, or set `STROLL_LSP_CONFIG` to its path.
Use the standard JSON-RPC/LSP stdio transport. The suffix map supplies the
LSP language identifier. A root marker can match a relative path or a
filename glob. The nearest matching ancestor becomes the project root.

```json
[
  {
    "id": "example",
    "languages": {".example": "example"},
    "rootMarkers": ["example.toml"],
    "server": {
      "command": "/absolute/path/example-lsp",
      "args": ["--stdio"],
      "initializationOptions": {}
    }
  }
]
```

Optional `server.env` entries set environment variables for the server.
Use `server.initializationOptions` for the LSP initialize request and an
optional `server.workspaceSettings` object for the server's workspace settings.
For SSH, install the updated remote helper and register the same server ID
in the helper user's `lsp.json` with the command for that host. The client
routes by ID and the helper resolves the executable from its own configuration.

### Markdown with Marksman

Place the `marksman` executable on `PATH` and add this entry to the external
LSP configuration array. Use F12 in the Markdown source editor to follow links.
Inline file links and heading anchors also work without a language server.
Marksman provides reference-link navigation and link completion in the editor's
completion menu.

```json
{
  "id": "marksman",
  "languages": {".md": "markdown", ".markdown": "markdown"},
  "rootMarkers": [".marksman.toml"],
  "server": {"command": "marksman", "args": ["server"]}
}
```

### JSON, YAML, and TOML

Install the VS Code JSON language server (available in
`vscode-langservers-extracted`), Red Hat `yaml-language-server`, and Tombi.
Place their executables on `PATH` and add these entries to the external LSP
configuration array.

```json
[
  {
    "id": "json",
    "languages": {".json": "json", ".jsonc": "jsonc"},
    "server": {
      "command": "vscode-json-language-server",
      "args": ["--stdio"],
      "initializationOptions": {"provideFormatter": true}
    }
  },
  {
    "id": "yaml",
    "languages": {".yaml": "yaml", ".yml": "yaml"},
    "server": {
      "command": "yaml-language-server",
      "args": ["--stdio"],
      "workspaceSettings": {"yaml": {"format": {"enable": true}}}
    }
  },
  {
    "id": "tombi",
    "languages": {".toml": "toml"},
    "server": {"command": "tombi", "args": ["lsp"]}
  }
]
```

Use **Format Document** (`Alt+Shift+F`) in an editable source tab. Project
schemas can be declared with JSON's `$schema` field, YAML's
`# yaml-language-server: $schema=./schema.json` directive, or Tombi's
`#:schema ./schema.json` directive. Tombi reads its formatter configuration
from the project and offers plain-text schema completions with `Ctrl+Space`.
JSON and YAML provide diagnostics and manual formatting. YAML also shows any
plain-text candidates it receives. JSON completion and YAML snippet candidates
require client-side snippet support.
