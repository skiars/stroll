import EditorModel.Workspace

namespace EditorModel.Examples

private inductive File
  | first
  | second
  deriving DecidableEq

private def initial : Workspace File :=
  Workspace.open (fun
    | .first => "longKEYtail".toList
    | .second => "other".toList) .first ⟨.unified, 0, 3⟩

private def caret (offset : Nat) : SelectionRequest :=
  let position := Position.mk offset .right
  ⟨position, position⟩

private def offsets (state : Workspace File) (file : File) : List Nat :=
  (state.buffers file).selections.map fun selection => selection.head.offset.val

private def text (state : Workspace File) (file : File) : Text :=
  (state.buffers file).text

private def short : Workspace File :=
  initial.run [
    .edit [⟨7, 11, ['c']⟩, ⟨0, 4, ['a']⟩],
    .selectSource [caret 2]
  ]

-- The unchanged anchor is inside KEY. Undo changes both sides, then input
-- must use the anchor's new source position.
example : offsets (short.step .undo).state .first = [5] := by decide +kernel

example : text (short.run [.undo, .redo, .undo, .input ['!']]) .first = "longK!EYtail".toList := by
  decide +kernel

-- Deleting the current visible run does not change the redo target. The
-- second file provides a separate history to make that distinction observable.
private def hiddenAfterUndo : Workspace File :=
  let edited := initial.run [
    .externalEdit .second 0 [⟨0, 0, ['b']⟩],
    .edit [⟨0, 0, "draft\n".toList⟩],
    .selectSource [caret 2],
    .undo
  ]
  (edited.step (.publish ⟨edited.ticket, [.decoration ['…'], .source 4 11]⟩)).state

example : (hiddenAfterUndo.visiblePatch.bind (·.locate ⟨0, .right⟩)).isNone = true := by
  decide +kernel

example : offsets hiddenAfterUndo .first = [0] := by decide +kernel

example : (text (hiddenAfterUndo.step .redo).state .first,
    text (hiddenAfterUndo.step .redo).state .second) = ("draft\nlongKEYtail".toList, "bother".toList) := by
  decide +kernel

-- Installing more context and changing layout both preserve every logical
-- selection. This is true even for positions the original view omitted.
private def withTwoCarets : Workspace File :=
  let selected := (initial.step (.selectSource [caret 3, caret 6])).state
  let narrow := (selected.step (.publish ⟨selected.ticket, [.source 0 4]⟩)).state
  let split := (narrow.step (.configure ⟨.split, 0, 5⟩)).state
  (split.step (.publish ⟨split.ticket, [.source 0 11]⟩)).state

example : offsets withTwoCarets .first = [3, 6] := by decide +kernel

example : text (withTwoCarets.step (.input ['!'])).state .first = "lon!gKE!Ytail".toList := by
  decide +kernel

-- Reverse selections take part in the same batch as carets.
example : text (initial.run [
    .selectSource [⟨⟨7, .right⟩, ⟨4, .left⟩⟩, caret 2],
    .input ['_']]) .first = "lo_ng_tail".toList := by decide +kernel

-- A rejected second splice leaves the first splice unapplied.
example : (initial.step (.edit [⟨0, 0, ['!']⟩, ⟨99, 99, []⟩])).outcome =
    .invalidRange := by decide +kernel

example : text (initial.step (.edit [⟨0, 0, ['!']⟩, ⟨99, 99, []⟩])).state .first =
    "longKEYtail".toList := by decide +kernel

-- An edit followed by its inverse restores the text and still expires
-- the original asynchronous result and its input coordinates.
private def reverted : Workspace File :=
  initial.run [.edit [⟨0, 0, ['!']⟩], .undo]

example : reverted.ticket.revision = 2 := by decide +kernel

example : (reverted.step (.publish ⟨initial.ticket, [.source 0 11]⟩)).outcome =
    .stalePatch := by decide +kernel

example : (reverted.step (.selectView initial.ticket [])).outcome = .stalePatch := by decide +kernel

example : (initial.step (.publish ⟨initial.ticket, [.source 2 99]⟩)).outcome =
    .invalidPatch := by decide +kernel

example : (withTwoCarets.step (.publish ⟨withTwoCarets.ticket, [.source 4 7]⟩)).outcome =
    .noChange := by decide +kernel

end EditorModel.Examples
