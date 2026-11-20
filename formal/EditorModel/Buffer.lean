import EditorModel.Document

namespace EditorModel

/-- The stack indices ensure that the top of each history starts at the
    current text and that consecutive transactions meet at the same text. -/
inductive Past : Text → Type where
  | empty (text : Text) : Past text
  | push (past : Past before) (script : Script before after) : Past after

inductive Future : Text → Type where
  | empty (text : Text) : Future text
  | push (script : Script before after) (future : Future after) : Future before

structure Buffer where
  text : Text
  revision : Nat
  selections : List (Selection text.length)
  past : Past text
  future : Future text

def Buffer.open (text : Text) : Buffer :=
  ⟨text, 0, [], .empty text, .empty text⟩

def Buffer.applyScript (buffer : Buffer) (script : Script buffer.text after) : Buffer :=
  ⟨after, buffer.revision + 1, script.forwardSelections buffer.selections,
    .push buffer.past script, .empty after⟩

def Buffer.edit (buffer : Buffer) (splices : List Splice) : Option Buffer :=
  (Script.compile buffer.text splices).map fun ⟨_, script⟩ => buffer.applyScript script

structure Position where
  offset : Nat
  affinity : Affinity
  deriving Repr, DecidableEq

structure SelectionRequest where
  anchor : Position
  head : Position
  deriving Repr, DecidableEq

def Position.validate (position : Position) (length : Nat) : Option (Anchor length) :=
  if valid : position.offset < length + 1 then
    some ⟨⟨position.offset, valid⟩, position.affinity⟩
  else none

def SelectionRequest.validate (request : SelectionRequest) (length : Nat) :
    Option (Selection length) := do
  return ⟨← request.anchor.validate length, ← request.head.validate length⟩

def Buffer.select (buffer : Buffer) (requests : List SelectionRequest) : Option Buffer :=
  (requests.mapM fun request => SelectionRequest.validate request buffer.text.length).map fun selections =>
    { buffer with selections }

def Selection.inputSplice (selection : Selection length) (inserted : Text) : Splice :=
  ⟨min selection.anchor.offset.val selection.head.offset.val,
    max selection.anchor.offset.val selection.head.offset.val, inserted⟩

def Selection.collapseForInput (selection : Selection length) : Selection length :=
  let endpoint := if selection.anchor.offset ≤ selection.head.offset then selection.head.offset
    else selection.anchor.offset
  let caret : Anchor length := ⟨endpoint, .right⟩
  ⟨caret, caret⟩

/-- Input ranges are checked in descending order, so each splice can use
    coordinates from the original buffer. Ranges that only touch are accepted.
    Coincident starting positions are rejected as ambiguous. -/
private def separatedFrom (higher : Splice) : List Splice → Bool
  | [] => true
  | lower :: rest =>
    decide (lower.start < higher.start ∧ lower.stop ≤ higher.start) &&
      separatedFrom lower rest

def separated : List Splice → Bool
  | [] => true
  | higher :: rest => separatedFrom higher rest

private def insertDescending (splice : Splice) : List Splice → List Splice
  | [] => [splice]
  | first :: rest =>
    if first.start ≤ splice.start then splice :: first :: rest
    else first :: insertDescending splice rest

def Buffer.input (buffer : Buffer) (inserted : Text) : Option Buffer :=
  let splices := buffer.selections.foldr (fun s => insertDescending (s.inputSplice inserted)) []
  if separated splices then
    if splices.isEmpty then some buffer
    else
      let collapsed := buffer.selections.map Selection.collapseForInput
      ({ buffer with selections := collapsed } : Buffer).edit splices
  else none

def Buffer.undo : Buffer → Option Buffer
  | ⟨_, _, _, .empty _, _⟩ => none
  | ⟨_, revision, selections, @Past.push before _ past script, future⟩ =>
    some ⟨before, revision + 1, script.backwardSelections selections, past, .push script future⟩

def Buffer.redo : Buffer → Option Buffer
  | ⟨_, _, _, _, .empty _⟩ => none
  | ⟨_, revision, selections, past, @Future.push _ after script future⟩ =>
    some ⟨after, revision + 1, script.forwardSelections selections, .push past script, future⟩

def Buffer.history (buffer : Buffer) : Σ text, Past text × Future text :=
  ⟨buffer.text, buffer.past, buffer.future⟩

variable {buffer undone redone : Buffer}
theorem Buffer.undo_then_redo (undo : buffer.undo = some undone) :
    ∃ redone, undone.redo = some redone ∧ redone.history = buffer.history ∧
      redone.revision = buffer.revision + 2 ∧
      redone.selections.length = buffer.selections.length := by
  cases buffer with
  | mk text revision selections past future =>
    cases past with
    | empty => simp [Buffer.undo] at undo
    | push past script =>
      simp [Buffer.undo] at undo
      subst undone
      exact ⟨_, rfl, rfl, rfl, by simp⟩

theorem Buffer.redo_then_undo (redo : buffer.redo = some redone) :
    ∃ undone, redone.undo = some undone ∧ undone.history = buffer.history ∧
      undone.revision = buffer.revision + 2 ∧
      undone.selections.length = buffer.selections.length := by
  cases buffer with
  | mk text revision selections past future =>
    cases future with
    | empty => simp [Buffer.redo] at redo
    | push script future =>
      simp [Buffer.redo] at redo
      subst redone
      exact ⟨_, rfl, rfl, rfl, by simp⟩

@[simp] theorem Buffer.applyScript_clears_redo (buffer : Buffer)
    (script : Script buffer.text after) :
    (buffer.applyScript script).future = .empty after := rfl

@[simp] theorem Buffer.applyScript_increments_revision (buffer : Buffer)
    (script : Script buffer.text after) :
    (buffer.applyScript script).revision = buffer.revision + 1 := rfl

theorem Buffer.undo_increments_revision (h : buffer.undo = some undone) :
    undone.revision = buffer.revision + 1 := by
  cases buffer with
  | mk text revision selections past future =>
    cases past <;> simp [Buffer.undo] at h <;> subst undone <;> rfl

theorem Buffer.redo_increments_revision (h : buffer.redo = some redone) :
    redone.revision = buffer.revision + 1 := by
  cases buffer with
  | mk text revision selections past future =>
    cases future <;> simp [Buffer.redo] at h <;> subst redone <;> rfl

end EditorModel
