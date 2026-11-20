import EditorModel.Buffer

namespace EditorModel

inductive Layout where
  | source
  | unified
  | split
  deriving Repr, DecidableEq

/-- Comparison and context are configuration, so changes to either expire
    outstanding work even if the source text and the split mode are unchanged. -/
structure ViewConfig where
  layout : Layout
  comparison : Nat
  context : Nat
  deriving Repr, DecidableEq

structure Ticket (File : Type) where
  file : File
  revision : Nat
  generation : Nat
  deriving Repr, DecidableEq

inductive Piece where
  | source (start stop : Nat)
  | decoration (text : Text)
  deriving Repr, DecidableEq

def Piece.valid (length : Nat) : Piece → Bool
  | .source start stop => decide (start ≤ stop ∧ stop ≤ length)
  | .decoration _ => true

def Piece.render (text : Text) : Piece → Text
  | .source start stop => (text.drop start).take (stop - start)
  | .decoration text => text

/-- The piece index keeps adjacent ranges, and the sides of a fold, distinct.
    A displayed boundary in a decoration does not resolve to a source anchor. -/
structure ViewPosition where
  piece : Nat
  offset : Nat
  affinity : Affinity
  deriving Repr, DecidableEq

structure ViewSelection where
  anchor : ViewPosition
  head : ViewPosition
  deriving Repr, DecidableEq

structure Patch (File : Type) where
  ticket : Ticket File
  pieces : List Piece
  deriving Repr, DecidableEq

def Piece.resolve (piece : Piece) (offset : Nat) (affinity : Affinity) : Option Position :=
  match piece with
  | .decoration _ => none
  | .source start stop =>
    if start + offset ≤ stop then
      some ⟨start + offset, affinity⟩
    else none

def Patch.resolve (patch : Patch File) (position : ViewPosition) : Option Position :=
  patch.pieces[position.piece]?.bind fun piece => piece.resolve position.offset position.affinity

def Piece.locate (piece : Piece) (position : Position) : Option Nat :=
  match piece with
  | .decoration _ => none
  | .source start stop =>
    if start ≤ position.offset ∧ position.offset ≤ stop then some (position.offset - start)
    else none

def locateIn (position : Position) : List Piece → Option ViewPosition
  | [] => none
  | piece :: tail =>
    match piece.locate position with
    | some offset => some ⟨0, offset, position.affinity⟩
    | none => (locateIn position tail).map fun result => { result with piece := result.piece + 1 }

def Patch.locate (patch : Patch File) (position : Position) : Option ViewPosition :=
  locateIn position patch.pieces

theorem Piece.resolve_locate (piece : Piece) (position : Position) (offset : Nat)
    (found : piece.locate position = some offset) :
    piece.resolve offset position.affinity = some position := by
  cases position with
  | mk pos affinity =>
    cases piece with
    | decoration => simp [Piece.locate] at found
    | source start stop =>
      by_cases inside : start ≤ pos ∧ pos ≤ stop
      · simp [Piece.locate, inside] at found
        subst offset
        have reconstructed : start + (pos - start) = pos := by omega
        simp [Piece.resolve, reconstructed, inside.2]
      · simp [Piece.locate, inside] at found

private theorem resolve_locateIn (pieces : List Piece) (position : Position) (view : ViewPosition)
    (found : locateIn position pieces = some view) :
    (pieces[view.piece]?.bind fun piece => piece.resolve view.offset view.affinity) =
      some position := by
  induction pieces generalizing view with
  | nil => simp [locateIn] at found
  | cons piece tail ih =>
    cases here : piece.locate position with
    | some offset =>
      simp [locateIn, here] at found
      subst view
      simpa using piece.resolve_locate position offset here
    | none =>
      cases there : locateIn position tail with
      | none => simp [locateIn, here, there] at found
      | some next =>
        simp [locateIn, here, there] at found
        subst view
        simpa using ih next there

theorem Patch.resolve_locate (patch : Patch File) (position : Position) (view : ViewPosition)
    (found : patch.locate position = some view) :
    patch.resolve view = some position :=
  resolve_locateIn patch.pieces position view found

theorem Piece.resolve_bounded (piece : Piece) (length offset : Nat) (affinity : Affinity)
    (position : Position) (valid : piece.valid length = true)
    (found : piece.resolve offset affinity = some position) :
    position.offset ≤ length := by
  cases piece with
  | decoration => simp [Piece.resolve] at found
  | source start stop =>
    simp [Piece.valid] at valid
    by_cases inside : start + offset ≤ stop
    · simp [Piece.resolve, inside] at found
      subst position
      exact Nat.le_trans inside valid.2
    · simp [Piece.resolve, inside] at found

theorem Patch.resolve_bounded (patch : Patch File) (length : Nat) (view : ViewPosition)
    (position : Position) (valid : patch.pieces.all (Piece.valid length) = true)
    (found : patch.resolve view = some position) :
    position.offset ≤ length := by
  cases cell : patch.pieces[view.piece]? with
  | none => simp [Patch.resolve, cell] at found
  | some piece =>
    have pieceValid := List.all_eq_true.mp valid piece (List.mem_of_getElem? cell)
    have resolved : piece.resolve view.offset view.affinity = some position := by
      simpa [Patch.resolve, cell] using found
    exact piece.resolve_bounded length view.offset view.affinity position pieceValid resolved

/-- The file type describes the workspace's open buffers. The history target
    is retained even when its current projection has no editable rows. -/
structure Workspace (File : Type) where
  buffers : File → Buffer
  target : File
  config : ViewConfig
  generation : Nat
  patch : Option (Patch File)

def Workspace.open (files : File → Text) (target : File) (config : ViewConfig) :
    Workspace File :=
  ⟨fun file => Buffer.open (files file), target, config, 0, none⟩

def Workspace.ticket (workspace : Workspace File) : Ticket File :=
  ⟨workspace.target, (workspace.buffers workspace.target).revision, workspace.generation⟩

def Workspace.accepts [DecidableEq File] (workspace : Workspace File) (patch : Patch File) :
    Bool :=
  decide (patch.ticket = workspace.ticket) &&
    patch.pieces.all (Piece.valid (workspace.buffers workspace.target).text.length)

def Workspace.visiblePatch [DecidableEq File] (workspace : Workspace File) : Option (Patch File) :=
  workspace.patch.filter (workspace.accepts ·)

def Workspace.render [DecidableEq File] (workspace : Workspace File) : Option Text :=
  workspace.visiblePatch.map fun patch =>
    patch.pieces.flatMap (Piece.render (workspace.buffers workspace.target).text)

def Patch.resolveSelection (patch : Patch File) (selection : ViewSelection) :
    Option SelectionRequest := do
  return ⟨← patch.resolve selection.anchor, ← patch.resolve selection.head⟩

inductive Command (File : Type) where
  | focus (file : File)
  | configure (config : ViewConfig)
  | selectSource (ranges : List SelectionRequest)
  | selectView (ticket : Ticket File) (ranges : List ViewSelection)
  | input (text : Text)
  | edit (splices : List Splice)
  | externalEdit (file : File) (revision : Nat) (splices : List Splice)
  | undo
  | redo
  | publish (patch : Patch File)

inductive Outcome where
  | applied
  | noChange
  | invalidRange
  | staleRevision
  | stalePatch
  | invalidPatch
  deriving Repr, DecidableEq

structure Result (File : Type) where
  outcome : Outcome
  state : Workspace File

def Workspace.write [DecidableEq File] (workspace : Workspace File) (file : File)
    (buffer : Buffer) : Workspace File :=
  { workspace with buffers := fun key => if key = file then buffer else workspace.buffers key }

def Workspace.finish [DecidableEq File] (workspace : Workspace File) (file : File)
    (result : Option Buffer) (onNone : Outcome) : Result File :=
  match result with
  | none => ⟨onNone, workspace⟩
  | some buffer => ⟨.applied, workspace.write file buffer⟩

def Workspace.publish [DecidableEq File] (workspace : Workspace File) (patch : Patch File) :
    Result File :=
  if patch.ticket ≠ workspace.ticket then ⟨.stalePatch, workspace⟩
  else if !workspace.accepts patch then ⟨.invalidPatch, workspace⟩
  -- A ticket identifies one installed mapping. Competing replies for it
  -- cannot invalidate coordinates already issued by the view.
  else if workspace.visiblePatch.isSome then ⟨.noChange, workspace⟩
  else ⟨.applied, { workspace with patch := some patch }⟩

def Workspace.selectView [DecidableEq File] (workspace : Workspace File) (ticket : Ticket File)
    (ranges : List ViewSelection) : Result File :=
  if ticket ≠ workspace.ticket then ⟨.stalePatch, workspace⟩
  else match workspace.visiblePatch with
    | none => ⟨.stalePatch, workspace⟩
    | some patch =>
      let selected := do
        let requests ← ranges.mapM patch.resolveSelection
        (workspace.buffers workspace.target).select requests
      workspace.finish workspace.target selected .invalidRange

def Workspace.step [DecidableEq File] (workspace : Workspace File) : Command File → Result File
  | .focus file =>
    ⟨.applied, { workspace with target := file, generation := workspace.generation + 1 }⟩
  | .configure config =>
    ⟨.applied, { workspace with config, generation := workspace.generation + 1 }⟩
  | .selectSource ranges =>
    workspace.finish workspace.target ((workspace.buffers workspace.target).select ranges)
      .invalidRange
  | .selectView ticket ranges => workspace.selectView ticket ranges
  | .input text =>
    let buffer := workspace.buffers workspace.target
    if buffer.selections.isEmpty then ⟨.noChange, workspace⟩
    else workspace.finish workspace.target (buffer.input text) .invalidRange
  | .edit splices =>
    if splices.isEmpty then ⟨.noChange, workspace⟩
    else workspace.finish workspace.target ((workspace.buffers workspace.target).edit splices)
      .invalidRange
  | .externalEdit file revision splices =>
    let buffer := workspace.buffers file
    if buffer.revision ≠ revision then ⟨.staleRevision, workspace⟩
    else if splices.isEmpty then ⟨.noChange, workspace⟩
    else workspace.finish file (buffer.edit splices) .invalidRange
  | .undo =>
    workspace.finish workspace.target (workspace.buffers workspace.target).undo .noChange
  | .redo =>
    workspace.finish workspace.target (workspace.buffers workspace.target).redo .noChange
  | .publish patch => workspace.publish patch

def Workspace.run [DecidableEq File] (workspace : Workspace File) (commands : List (Command File)) :
    Workspace File :=
  commands.foldl (fun state cmd => (state.step cmd).state) workspace

section Proofs

variable {File : Type} [DecidableEq File]

theorem Workspace.write_isolated (workspace : Workspace File) (buffer : Buffer)
    {file other : File} (different : other ≠ file) :
    (workspace.write file buffer).buffers other = workspace.buffers other := by
  simp [Workspace.write, different]

theorem Workspace.finish_isolated (workspace : Workspace File) (buffer : Option Buffer)
    (onNone : Outcome) {file other : File} (different : other ≠ file) :
    (workspace.finish file buffer onNone).state.buffers other = workspace.buffers other := by
  cases buffer <;> simp [Workspace.finish, Workspace.write, different]

theorem Workspace.publish_preserves_logical_state (workspace : Workspace File)
    (patch : Patch File) :
    (workspace.publish patch).state.buffers = workspace.buffers ∧
      (workspace.publish patch).state.target = workspace.target := by
  unfold Workspace.publish
  split
  · exact ⟨rfl, rfl⟩
  · split
    · exact ⟨rfl, rfl⟩
    · split <;> exact ⟨rfl, rfl⟩

theorem Workspace.stale_patch_rejected (workspace : Workspace File) (patch : Patch File)
    (stale : patch.ticket ≠ workspace.ticket) :
    workspace.publish patch = ⟨.stalePatch, workspace⟩ := by
  simp [Workspace.publish, stale]

theorem Workspace.duplicate_patch_preserves_mapping (workspace : Workspace File)
    (patch : Patch File) (installed : workspace.visiblePatch.isSome = true) :
    (workspace.publish patch).state = workspace := by
  unfold Workspace.publish
  split
  · rfl
  · split
    · rfl
    · rfl

theorem Workspace.stale_view_selection_rejected (workspace : Workspace File)
    (ticket : Ticket File) (ranges : List ViewSelection) (stale : ticket ≠ workspace.ticket) :
    workspace.step (.selectView ticket ranges) = ⟨.stalePatch, workspace⟩ := by
  simp [Workspace.step, Workspace.selectView, stale]

theorem Workspace.stale_external_edit_rejected (workspace : Workspace File)
    (file : File) (revision : Nat) (splices : List Splice)
    (stale : (workspace.buffers file).revision ≠ revision) :
    workspace.step (.externalEdit file revision splices) = ⟨.staleRevision, workspace⟩ := by
  simp [Workspace.step, stale]

theorem Workspace.invalid_edit_rejected (workspace : Workspace File) (splices : List Splice)
    (invalid : ¬ validBatch (workspace.buffers workspace.target).text splices) :
    workspace.step (.edit splices) = ⟨.invalidRange, workspace⟩ := by
  cases splices with
  | nil => exact False.elim (invalid trivial)
  | cons splice tail =>
    have compileFailed :
        Script.compile (workspace.buffers workspace.target).text (splice :: tail) = none := by
      cases h : Script.compile (workspace.buffers workspace.target).text (splice :: tail) with
      | none => rfl
      | some script =>
        exact False.elim (invalid ((Script.compile_iff_valid _ _).mp (by simp [h])))
    simp [Workspace.step, Buffer.edit, compileFailed, Workspace.finish]

theorem Workspace.undo_isolated (workspace : Workspace File) (other : File)
    (different : other ≠ workspace.target) :
    (workspace.step .undo).state.buffers other = workspace.buffers other :=
  workspace.finish_isolated _ _ different

theorem Workspace.redo_isolated (workspace : Workspace File) (other : File)
    (different : other ≠ workspace.target) :
    (workspace.step .redo).state.buffers other = workspace.buffers other :=
  workspace.finish_isolated _ _ different

def Command.targetAfter (initial : File) : Command File → File
  | .focus file => file
  | _ => initial

@[simp] theorem Workspace.finish_target (workspace : Workspace File) (file : File)
    (buffer : Option Buffer) (onNone : Outcome) :
    (workspace.finish file buffer onNone).state.target = workspace.target := by
  cases buffer <;> rfl

theorem Workspace.selectView_target (workspace : Workspace File) (ticket : Ticket File)
    (ranges : List ViewSelection) :
    (workspace.selectView ticket ranges).state.target = workspace.target := by
  unfold Workspace.selectView
  split
  · rfl
  · split
    · rfl
    · simp

theorem Workspace.step_target (workspace : Workspace File) (command : Command File) :
    (workspace.step command).state.target = command.targetAfter workspace.target := by
  cases command <;> simp [Workspace.step, Command.targetAfter, Workspace.selectView_target,
    Workspace.publish_preserves_logical_state]
  all_goals repeat' first | rfl | apply Workspace.finish_target | split

theorem Workspace.run_target (workspace : Workspace File) (commands : List (Command File)) :
    (workspace.run commands).target =
      commands.foldl (fun target cmd => cmd.targetAfter target) workspace.target := by
  induction commands generalizing workspace with
  | nil => rfl
  | cons command rest ih =>
    simpa [Workspace.run, List.foldl, Workspace.step_target] using ih (workspace.step command).state

end Proofs
end EditorModel
