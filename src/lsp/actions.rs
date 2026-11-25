use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use gpui_component::input::{Rope, RopeExt};
use lsp_types::{
    CodeAction, CodeActionOrCommand, CodeActionProviderCapability, Command,
    DocumentChangeOperation, DocumentChanges, OneOf, Position, PrepareRenameResponse, RenameParams,
    ServerCapabilities, TextEdit, WorkspaceEdit,
};

use super::{Document, DocumentId, Query, Response, SessionState, lsp_position};

pub(super) const CLANGD_APPLY_TWEAK: &str = "clangd.applyTweak";

#[derive(Clone, Copy, Default)]
pub(crate) struct Capabilities {
    pub list: bool,
    pub resolve: bool,
    pub clangd_tweak: bool,
    pub rename: bool,
    pub prepare_rename: bool,
}

impl From<&ServerCapabilities> for Capabilities {
    fn from(capabilities: &ServerCapabilities) -> Self {
        Self {
            list: capabilities
                .code_action_provider
                .as_ref()
                .is_some_and(|cap| *cap != CodeActionProviderCapability::Simple(false)),
            resolve: matches!(&capabilities.code_action_provider,
                Some(CodeActionProviderCapability::Options(options)) if options.resolve_provider == Some(true)),
            clangd_tweak: supports_clangd_tweak(capabilities),
            rename: capabilities
                .rename_provider
                .as_ref()
                .is_some_and(|cap| *cap != OneOf::Left(false)),
            prepare_rename: matches!(&capabilities.rename_provider,
                Some(OneOf::Right(options)) if options.prepare_provider == Some(true)),
        }
    }
}

pub(super) fn supports_clangd_tweak(capabilities: &ServerCapabilities) -> bool {
    capabilities
        .execute_command_provider
        .as_ref()
        .is_some_and(|provider| {
            provider
                .commands
                .iter()
                .any(|name| name == CLANGD_APPLY_TWEAK)
        })
}

#[derive(Clone)]
pub(crate) struct ActionSnapshot {
    pub(super) source: DocumentId,
    pub version: i32,
    pub documents: HashMap<String, (i32, Rope)>,
}

impl Response {
    fn action_snapshot(&mut self) -> Result<ActionSnapshot> {
        self.snapshot
            .take()
            .context("The language service returned no document version")
    }
}

impl Document {
    pub(crate) fn format_document(
        &self,
        text: Rope,
        options: lsp_types::FormattingOptions,
    ) -> impl Future<Output = Result<Option<TextEdits>>> + use<> {
        // Enqueue before yielding so a later editor change cannot be queued first.
        let request = self.begin_request(Query::Format(options), text.clone());
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            if response.snapshot.is_none() {
                return Ok(None);
            }
            let edits = serde_json::from_value::<Option<Vec<TextEdit>>>(response.value)?;
            validated_text_edits(&text, edits.unwrap_or_default()).map(Some)
        }
    }

    pub(crate) fn action_capabilities(&self) -> Capabilities {
        if self.client.state() != SessionState::Ready {
            return Capabilities::default();
        }
        *self.client.0.actions.lock().unwrap()
    }

    pub(crate) fn action_uri(&self) -> &str {
        &self.key.uri
    }

    pub(crate) async fn code_actions(
        &self,
        text: Rope,
        range: Range<usize>,
    ) -> Result<(Vec<CodeAction>, ActionSnapshot)> {
        ensure!(
            range.start <= range.end
                && range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end),
            "Invalid code action range"
        );
        let range = lsp_types::Range::new(
            lsp_position(&text, range.start),
            lsp_position(&text, range.end),
        );
        let mut response = self.request(Query::CodeActions(range), text).await?;
        let snapshot = response.action_snapshot()?;
        let actions = serde_json::from_value::<Option<Vec<CodeActionOrCommand>>>(response.value)?;
        let mut actions = actions
            .unwrap_or_default()
            .into_iter()
            .take(100)
            .map(|item| match item {
                CodeActionOrCommand::CodeAction(action) => action,
                CodeActionOrCommand::Command(command) => CodeAction {
                    title: command.title.clone(),
                    command: Some(command),
                    ..Default::default()
                },
            })
            .collect::<Vec<_>>();
        actions.sort_by_key(|item| !item.is_preferred.unwrap_or_default());
        actions.extend(response.supplemental_code_actions);
        Ok((actions, snapshot))
    }

    pub(crate) async fn resolve_action(
        &self,
        text: Rope,
        action: CodeAction,
        version: i32,
    ) -> Result<CodeAction> {
        let response = self
            .request(Query::ResolveAction(action, version), text)
            .await?;
        Ok(serde_json::from_value(response.value)?)
    }

    pub(crate) async fn clangd_tweak(
        &self,
        text: Rope,
        command: Command,
        version: i32,
    ) -> Result<WorkspaceEdit> {
        let response = self
            .request(Query::ClangdTweak(command, version), text)
            .await?;
        serde_json::from_value::<Option<WorkspaceEdit>>(response.value)?
            .context("This action has no text changes")
    }

    pub(crate) async fn prepare_rename(
        &self,
        text: Rope,
        offset: usize,
    ) -> Result<Option<PrepareRenameResponse>> {
        let position = lsp_position(&text, offset);
        let response = self.request(Query::PrepareRename(position), text).await?;
        Ok(serde_json::from_value(response.value)?)
    }

    pub(crate) async fn rename(
        &self,
        text: Rope,
        position: Position,
        name: String,
        version: Option<i32>,
    ) -> Result<(WorkspaceEdit, ActionSnapshot)> {
        let mut response = self
            .request(Query::Rename(position, name, version), text)
            .await?;
        let snapshot = response.action_snapshot()?;
        let edit = serde_json::from_value::<Option<WorkspaceEdit>>(response.value)?
            .context("This action has no text changes")?;
        Ok((edit, snapshot))
    }
}

pub(crate) fn stale() -> anyhow::Error {
    anyhow!("This action is out of date. Request code actions again.")
}

pub(crate) fn unpack_clangd_fix(action: &mut CodeAction) {
    if action.edit.is_none()
        && let Some(command) = &action.command
        && command.command == "clangd.applyFix"
        && let Some([edit]) = command.arguments.as_deref()
        && let Ok(edit) = serde_json::from_value::<WorkspaceEdit>(edit.clone())
    {
        action.edit = Some(edit);
        action.command = None;
    }
}

pub(crate) fn clangd_tweak(command: &Command, uri: &str, text: &Rope) -> bool {
    #[derive(serde::Deserialize)]
    struct Tweak {
        file: String,
        selection: lsp_types::Range,
        #[serde(rename = "tweakID")]
        id: String,
    }

    let Some([argument]) = command.arguments.as_deref() else {
        return false;
    };
    command.command == CLANGD_APPLY_TWEAK
        && serde_json::from_value::<Tweak>(argument.clone()).is_ok_and(|tweak| {
            tweak.file == uri
                && !tweak.id.is_empty()
                && tweak.selection.start <= tweak.selection.end
                && byte_offset(text, tweak.selection.start).is_some()
                && byte_offset(text, tweak.selection.end).is_some()
        })
}

pub(crate) fn clangd_rename(command: &Command, uri: &str, text: &Rope) -> Option<RenameParams> {
    if command.command != "clangd.applyRename" {
        return None;
    }
    let [argument] = command.arguments.as_deref()? else {
        return None;
    };
    let params: RenameParams = serde_json::from_value(argument.clone()).ok()?;
    let target = &params.text_document_position;
    (target.text_document.uri.as_str() == uri
        && byte_offset(text, target.position).is_some()
        && valid_name(&params.new_name))
    .then_some(params)
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 1024 && !name.contains(['\n', '\r', '\0'])
}

pub(crate) fn typescript_noop(command: &Command) -> bool {
    command.command == "_typescript.applyCodeActionCommand"
        && matches!(command.arguments.as_deref(), Some([argument])
            if argument.get("action").and_then(|action| action.as_object()).is_some_and(|action|
                action.get("commands").is_none_or(|commands| commands.as_array().is_some_and(Vec::is_empty))))
}

pub(crate) struct EditTarget {
    pub(crate) version: Option<i32>,
    pub(crate) edits: Vec<TextEdit>,
}

pub(crate) fn workspace_edits(
    edit: WorkspaceEdit,
    source: Option<&str>,
) -> Result<BTreeMap<String, EditTarget>> {
    let annotations = edit.change_annotations.unwrap_or_default();
    ensure!(
        !annotations
            .values()
            .any(|annotation| annotation.needs_confirmation == Some(true)),
        "This action requires change confirmation"
    );
    let mut targets = BTreeMap::new();
    let mut add = |uri: String, version, edits| {
        ensure!(
            source.is_none_or(|source| uri == source),
            "This action changes other files"
        );
        ensure!(
            targets.insert(uri, EditTarget { version, edits }).is_none(),
            "The edit lists a file more than once"
        );
        Ok(())
    };
    for (uri, edits) in edit.changes.into_iter().flatten() {
        add(uri.to_string(), None, edits)?;
    }
    if let Some(changes) = edit.document_changes {
        let changes = match changes {
            DocumentChanges::Edits(edits) => edits,
            DocumentChanges::Operations(ops) => ops
                .into_iter()
                .map(|op| match op {
                    DocumentChangeOperation::Edit(edit) => Ok(edit),
                    DocumentChangeOperation::Op(_) => {
                        bail!("This action creates, renames or deletes files")
                    }
                })
                .collect::<Result<_>>()?,
        };
        for change in changes {
            let edits = change
                .edits
                .into_iter()
                .map(|edit| match edit {
                    OneOf::Left(edit) => Ok(edit),
                    OneOf::Right(edit) if annotations.contains_key(&edit.annotation_id) => {
                        Ok(edit.text_edit)
                    }
                    OneOf::Right(_) => bail!("Invalid change annotation"),
                })
                .collect::<Result<_>>()?;
            add(
                change.text_document.uri.to_string(),
                change.text_document.version,
                edits,
            )?;
        }
    }
    ensure!(
        targets.len() <= 128
            && targets
                .values()
                .map(|target| target.edits.len())
                .sum::<usize>()
                <= 10_000,
        "This action has too many edits"
    );
    Ok(targets)
}

pub(crate) fn byte_offset(text: &Rope, position: Position) -> Option<usize> {
    let row = position.line as usize;
    if row >= text.lines_len() {
        return None;
    }
    let line = text.slice_line(row);
    let column = position.character as usize;
    if column > line.len_utf16() {
        return None;
    }
    let offset = line.utf16_to_byte_idx(column);
    (line.byte_to_utf16_idx(offset) == column
        && !(offset == line.len() && offset > 0 && line.byte(offset - 1) == b'\r'))
        .then(|| text.line_start_offset(row) + offset)
}

pub(crate) type TextEdits = Vec<(Range<usize>, String)>;

pub(crate) fn text_edits(
    text: &Rope,
    target: EditTarget,
    version: Option<i32>,
) -> Result<TextEdits> {
    ensure!(
        target.version.is_none() || target.version == version,
        "This action is out of date"
    );
    validated_text_edits(text, target.edits)
}

pub(super) fn validated_text_edits(text: &Rope, edits: Vec<TextEdit>) -> Result<TextEdits> {
    let mut edits = edits
        .into_iter()
        .map(|edit| {
            let invalid = || anyhow!("Invalid language server text edits");
            let start = byte_offset(text, edit.range.start).ok_or_else(invalid)?;
            let end = byte_offset(text, edit.range.end).ok_or_else(invalid)?;
            let replacement = edit.new_text.replace("\r\n", "\n");
            ensure!(
                start <= end && !replacement.contains(['\r', '\0']),
                "Invalid language server text edits"
            );
            Ok((start..end, replacement))
        })
        .collect::<Result<Vec<_>>>()?;
    edits.sort_by_key(|(range, _)| (range.start, range.end));
    // LSP permits multiple inserts at the same position in response order.
    let mut merged: TextEdits = Vec::new();
    for (range, replacement) in edits {
        if let Some((previous, previous_text)) = merged.last_mut() {
            ensure!(
                previous.end <= range.start,
                "The language server returned overlapping text edits"
            );
            if previous.start == previous.end && previous.start == range.start {
                previous.end = range.end;
                previous_text.push_str(&replacement);
                continue;
            }
        }
        merged.push((range, replacement));
    }
    merged.retain(|(range, replacement)| text.slice(range.clone()) != replacement.as_str());
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_utf16_edits_and_versions_before_apply() -> Result<()> {
        let text = Rope::from("😀old\r\nnext\n");
        assert_eq!(byte_offset(&text, Position::new(0, 1)), None);
        assert_eq!(byte_offset(&text, Position::new(0, 5)), Some(7));
        assert_eq!(byte_offset(&text, Position::new(0, 6)), None);
        assert_eq!(byte_offset(&text, Position::new(1, 0)), Some(9));

        let point = lsp_types::Range::new(Position::new(0, 2), Position::new(0, 2));
        let word = lsp_types::Range::new(point.start, Position::new(0, 5));
        let edits = vec![
            TextEdit::new(point, "first ".into()),
            TextEdit::new(point, "second ".into()),
            TextEdit::new(word, "new".into()),
        ];
        assert!(
            text_edits(
                &text,
                EditTarget {
                    version: Some(3),
                    edits: edits.clone()
                },
                Some(2)
            )
            .is_err()
        );
        assert_eq!(
            text_edits(
                &text,
                EditTarget {
                    version: Some(3),
                    edits
                },
                Some(3)
            )?,
            vec![(4..7, "first second new".into())]
        );
        assert!(
            text_edits(
                &text,
                EditTarget {
                    version: None,
                    edits: vec![
                        TextEdit::new(word, "A".into()),
                        TextEdit::new(word, "B".into())
                    ],
                },
                None
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn rejects_file_operations_and_unconfirmed_annotations() -> Result<()> {
        let resource: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [{"kind": "delete", "uri": "file:///repo/a"}]
        }))?;
        assert!(workspace_edits(resource, None).is_err());
        let needs_confirmation: WorkspaceEdit = serde_json::from_value(json!({
            "changeAnnotations": {
                "confirm": { "label": "Impact", "needsConfirmation": true }
            },
            "changes": {"file:///repo/a": []}
        }))?;
        assert!(workspace_edits(needs_confirmation, None).is_err());
        let cross_file: WorkspaceEdit = serde_json::from_value(json!({
            "changes": {"file:///repo/a": [], "file:///repo/b": []}
        }))?;
        assert!(workspace_edits(cross_file.clone(), Some("file:///repo/a")).is_err());
        assert_eq!(workspace_edits(cross_file, None)?.len(), 2);
        Ok(())
    }
}
