import EditorModel.Workspace

namespace EditorModel

/-- Revisions never decrease. Equal revisions of the same buffer identify the
    same text, even when selections have changed. -/
def Buffer.RevisionCoherent (before after : Buffer) : Prop :=
  before.revision ≤ after.revision ∧
    (before.revision = after.revision → before.text = after.text)

theorem Buffer.RevisionCoherent.refl (buffer : Buffer) : buffer.RevisionCoherent buffer :=
  ⟨Nat.le_refl _, fun _ => rfl⟩

theorem Buffer.RevisionCoherent.trans {a b c : Buffer} (ab : a.RevisionCoherent b)
    (bc : b.RevisionCoherent c) : a.RevisionCoherent c := by
  refine ⟨Nat.le_trans ab.1 bc.1, fun eq => ?_⟩
  have ab_eq : a.revision = b.revision := by have := ab.1; have := bc.1; omega
  exact (ab.2 ab_eq).trans (bc.2 (ab_eq.symm.trans eq))

private theorem coherentOfIncrease {before after : Buffer}
    (increase : after.revision = before.revision + 1) : before.RevisionCoherent after := by
  exact ⟨by omega, fun same => False.elim (by omega)⟩

theorem Buffer.edit_coherent {before after : Buffer} {splices : List Splice}
    (ok : before.edit splices = some after) : before.RevisionCoherent after := by
  cases compiled : Script.compile before.text splices with
  | none => simp [Buffer.edit, compiled] at ok
  | some plan =>
    cases plan with
    | mk result script =>
      simp [Buffer.edit, compiled] at ok
      subst after
      exact coherentOfIncrease rfl

theorem Buffer.select_coherent {before after : Buffer} {ranges : List SelectionRequest}
    (ok : before.select ranges = some after) : before.RevisionCoherent after := by
  cases checked : ranges.mapM (fun range => SelectionRequest.validate range before.text.length) with
  | none => simp [Buffer.select, checked] at ok
  | some selections =>
    simp [Buffer.select, checked] at ok
    subst after
    exact ⟨Nat.le_refl _, fun _ => rfl⟩

theorem Buffer.input_coherent {before after : Buffer} {text : Text}
    (ok : before.input text = some after) : before.RevisionCoherent after := by
  simp only [Buffer.input] at ok
  split at ok
  · split at ok
    · simp at ok
      subst after
      exact .refl before
    · exact Buffer.edit_coherent
        (before := { before with selections := before.selections.map Selection.collapseForInput }) ok
  · simp at ok

def Workspace.RevisionCoherent (before after : Workspace File) : Prop :=
  ∀ file, (before.buffers file).RevisionCoherent (after.buffers file)

theorem Workspace.RevisionCoherent.refl (workspace : Workspace File) :
    workspace.RevisionCoherent workspace := fun _ => .refl _

theorem Workspace.RevisionCoherent.trans {a b c : Workspace File}
    (ab : a.RevisionCoherent b) (bc : b.RevisionCoherent c) : a.RevisionCoherent c :=
  fun file => (ab file).trans (bc file)

variable {File : Type} [DecidableEq File]

theorem Workspace.finish_coherent (workspace : Workspace File) (file : File)
    (result : Option Buffer) (failure : Outcome)
    (consistent : ∀ buffer, result = some buffer → (workspace.buffers file).RevisionCoherent buffer) :
    workspace.RevisionCoherent (workspace.finish file result failure).state := by
  intro other
  cases result with
  | none => exact .refl _
  | some buffer =>
    by_cases same : other = file
    · subst other
      simpa [Workspace.finish, Workspace.write] using consistent buffer rfl
    · simpa [Workspace.finish, Workspace.write, same] using
        Buffer.RevisionCoherent.refl (workspace.buffers other)

theorem Workspace.selectView_coherent (workspace : Workspace File) (ticket : Ticket File)
    (ranges : List ViewSelection) :
    workspace.RevisionCoherent (workspace.selectView ticket ranges).state := by
  unfold Workspace.selectView
  split
  · exact .refl _
  · cases loaded : workspace.visiblePatch with
    | none => exact .refl _
    | some patch =>
      apply Workspace.finish_coherent
      intro buffer accepted
      cases resolved : ranges.mapM patch.resolveSelection with
      | none => simp [resolved] at accepted
      | some selections =>
        simp [resolved] at accepted
        exact Buffer.select_coherent accepted

theorem Workspace.step_coherent (workspace : Workspace File) (command : Command File) :
    workspace.RevisionCoherent (workspace.step command).state := by
  cases command with
  | focus => exact .refl _
  | configure => exact .refl _
  | selectSource =>
    exact workspace.finish_coherent _ _ _ (fun _ => Buffer.select_coherent)
  | selectView ticket ranges => exact workspace.selectView_coherent ticket ranges
  | input =>
    simp only [Workspace.step]
    split
    · exact .refl _
    · exact workspace.finish_coherent _ _ _ (fun _ => Buffer.input_coherent)
  | edit =>
    simp only [Workspace.step]
    split
    · exact .refl _
    · exact workspace.finish_coherent _ _ _ (fun _ => Buffer.edit_coherent)
  | externalEdit =>
    simp only [Workspace.step]
    split
    · exact .refl _
    · split
      · exact .refl _
      · exact workspace.finish_coherent _ _ _ (fun _ => Buffer.edit_coherent)
  | undo =>
    exact workspace.finish_coherent _ _ _
      (fun _ h => coherentOfIncrease (Buffer.undo_increments_revision h))
  | redo =>
    exact workspace.finish_coherent _ _ _
      (fun _ h => coherentOfIncrease (Buffer.redo_increments_revision h))
  | publish patch =>
    intro file
    change (workspace.buffers file).RevisionCoherent ((workspace.publish patch).state.buffers file)
    rw [(workspace.publish_preserves_logical_state patch).1]
    exact .refl _

theorem Workspace.run_coherent (workspace : Workspace File) (commands : List (Command File)) :
    workspace.RevisionCoherent (workspace.run commands) := by
  induction commands generalizing workspace with
  | nil => exact .refl _
  | cons command remaining ih =>
    exact (workspace.step_coherent command).trans (ih (workspace.step command).state)

theorem Workspace.finish_unchanged (workspace : Workspace File) (file : File)
    (result : Option Buffer) (failure : Outcome)
    (blocked : (workspace.finish file result failure).outcome ≠ .applied) :
    (workspace.finish file result failure).state = workspace := by
  cases result with
  | none => rfl
  | some => exact False.elim (blocked rfl)

theorem Workspace.step_unchanged (workspace : Workspace File) (command : Command File)
    (blocked : (workspace.step command).outcome ≠ .applied) :
    (workspace.step command).state = workspace := by
  cases command with
  | focus => exact False.elim (blocked rfl)
  | configure => exact False.elim (blocked rfl)
  | selectSource => exact workspace.finish_unchanged _ _ _ blocked
  | selectView ticket ranges =>
    by_cases stale : ticket ≠ workspace.ticket
    · simp [Workspace.step, Workspace.selectView, stale]
    · cases loaded : workspace.visiblePatch with
      | none => simp [Workspace.step, Workspace.selectView, stale, loaded]
      | some patch =>
        simp only [Workspace.step, Workspace.selectView, stale, loaded, ↓reduceIte] at blocked ⊢
        exact workspace.finish_unchanged _ _ _ blocked
  | input text =>
    by_cases empty : (workspace.buffers workspace.target).selections.isEmpty = true
    · simp [Workspace.step, empty]
    · simp only [Workspace.step, empty] at blocked ⊢
      exact workspace.finish_unchanged _ _ _ blocked
  | edit splices =>
    by_cases empty : splices.isEmpty = true
    · simp [Workspace.step, empty]
    · simp only [Workspace.step, empty] at blocked ⊢
      exact workspace.finish_unchanged _ _ _ blocked
  | externalEdit file revision splices =>
    by_cases stale : (workspace.buffers file).revision ≠ revision
    · simp [Workspace.step, stale]
    · by_cases empty : splices.isEmpty = true
      · simp [Workspace.step, stale, empty]
      · simp only [Workspace.step, stale, empty, ↓reduceIte] at blocked ⊢
        exact workspace.finish_unchanged _ _ _ blocked
  | undo => exact workspace.finish_unchanged _ _ _ blocked
  | redo => exact workspace.finish_unchanged _ _ _ blocked
  | publish patch =>
    simp only [Workspace.step, Workspace.publish] at blocked ⊢
    split
    · rfl
    · rename_i current
      split
      · rfl
      · rename_i valid
        split
        · rfl
        · rename_i newMapping
          exact False.elim (blocked (by simp [current, valid, newMapping]))

theorem Workspace.visible_patch_is_current (workspace : Workspace File) (patch : Patch File)
    (visible : workspace.visiblePatch = some patch) :
    patch.ticket = workspace.ticket ∧
      patch.pieces.all (Piece.valid (workspace.buffers workspace.target).text.length) = true := by
  cases stored : workspace.patch with
  | none => simp [Workspace.visiblePatch, stored] at visible
  | some candidate =>
    by_cases accepted : workspace.accepts candidate = true
    · simp [Workspace.visiblePatch, stored, Option.filter, accepted] at visible
      subst patch
      simpa [Workspace.accepts] using accepted
    · simp [Workspace.visiblePatch, stored, Option.filter, accepted] at visible

end EditorModel
