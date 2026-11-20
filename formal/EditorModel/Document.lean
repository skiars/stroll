import Std

namespace EditorModel

abbrev Text := List Char

inductive Affinity where
  | left
  | right
  deriving Repr, DecidableEq

/-- A source position is a boundary between characters, including EOF. -/
structure Anchor (length : Nat) where
  offset : Fin (length + 1)
  affinity : Affinity
  deriving Repr, DecidableEq

/-- Anchor and head are oriented. The head is the caret. -/
structure Selection (length : Nat) where
  anchor : Anchor length
  head : Anchor length
  deriving Repr, DecidableEq

structure Splice where
  start : Nat
  stop : Nat
  inserted : Text
  deriving Repr, DecidableEq

def Splice.Valid (s : Splice) (text : Text) : Prop :=
  s.start ≤ s.stop ∧ s.stop ≤ text.length

instance (s : Splice) (text : Text) : Decidable (s.Valid text) :=
  inferInstanceAs (Decidable (s.start ≤ s.stop ∧ s.stop ≤ text.length))

def Splice.apply (s : Splice) (text : Text) : Text :=
  text.take s.start ++ s.inserted ++ text.drop s.stop

theorem Splice.length_apply {s : Splice} {text : Text} (h : s.Valid text) :
    (s.apply text).length = s.start + s.inserted.length + (text.length - s.stop) := by
  obtain ⟨order, bound⟩ := h
  simp [Splice.apply, Nat.min_eq_left (Nat.le_trans order bound), Nat.add_assoc]

/-- Translate a boundary through a replacement. Boundaries on either side
    retain their distance from the replacement. At an insertion or inside a
    replaced region, affinity chooses the left or right edge. -/
def translate (start stop inserted pos : Nat) (affinity : Affinity) : Nat :=
  if pos < start then pos
  else if stop < pos ∨ (start < stop ∧ stop = pos) then start + inserted + (pos - stop)
  else match affinity with
    | .left => start
    | .right => start + inserted

theorem translate_prefix (h : pos < start) :
    translate start stop inserted pos affinity = pos := by simp [translate, h]

theorem translate_suffix (order : start ≤ stop) (h : stop < pos) :
    translate start stop inserted pos affinity = start + inserted + (pos - stop) := by
  have h' : ¬ pos < start := by omega
  simp [translate, h', h]

theorem translate_suffix_roundtrip (order : start ≤ stop) (h : stop < pos) :
    translate start (start + inserted) (stop - start)
      (translate start stop inserted pos affinity) affinity = pos := by
  rw [translate_suffix order h, translate_suffix (by omega) (by omega)]
  omega

theorem translate_prefix_roundtrip (h : pos < start) :
    translate start (start + inserted) (stop - start)
      (translate start stop inserted pos affinity) affinity = pos := by
  rw [translate_prefix h, translate_prefix h]

theorem translate_bounded (_order : start ≤ stop) (bound : stop ≤ length)
    (position : pos ≤ length) :
    translate start stop inserted pos affinity ≤ start + inserted + (length - stop) := by
  unfold translate
  split
  · omega
  · split
    · omega
    · cases affinity <;> dsimp <;> omega

def Splice.forward (s : Splice) {text : Text} (h : s.Valid text)
    (a : Anchor text.length) : Anchor (s.apply text).length :=
  ⟨⟨translate s.start s.stop s.inserted.length a.offset.val a.affinity, by
    have bound := translate_bounded h.1 h.2 (Nat.le_of_lt_succ a.offset.isLt)
      (affinity := a.affinity) (inserted := s.inserted.length)
    rw [s.length_apply h]
    omega⟩, a.affinity⟩

def Splice.backward (s : Splice) {text : Text} (h : s.Valid text)
    (a : Anchor (s.apply text).length) : Anchor text.length :=
  ⟨⟨translate s.start (s.start + s.inserted.length) (s.stop - s.start)
      a.offset.val a.affinity, by
    have order := h.1
    have oldBound := h.2
    have len := s.length_apply h
    have newBound : s.start + s.inserted.length ≤ (s.apply text).length := by omega
    have bound := translate_bounded (by omega) newBound (Nat.le_of_lt_succ a.offset.isLt)
      (start := s.start) (inserted := s.stop - s.start) (affinity := a.affinity)
    omega⟩, a.affinity⟩

/-- Every edge is checked against the text produced by its predecessor.
    Compiling a batch is pure, so an invalid intermediate splice rejects all of it. -/
inductive Script : Text → Text → Type where
  | done (text : Text) : Script text text
  | next {before after : Text} (splice : Splice) (valid : splice.Valid before)
      (tail : Script (splice.apply before) after) : Script before after

def Script.compile (before : Text) : List Splice → Option (Σ after, Script before after)
  | [] => some ⟨before, .done before⟩
  | splice :: tail =>
    if valid : splice.Valid before then
      (compile (splice.apply before) tail).map fun ⟨after, script⟩ =>
        ⟨after, .next splice valid script⟩
    else none

def validBatch (before : Text) : List Splice → Prop
  | [] => True
  | splice :: tail => splice.Valid before ∧ validBatch (splice.apply before) tail

theorem Script.compile_iff_valid (before : Text) (batch : List Splice) :
    (Script.compile before batch).isSome = true ↔ validBatch before batch := by
  induction batch generalizing before with
  | nil => simp [Script.compile, validBatch]
  | cons splice tail ih =>
    by_cases h : splice.Valid before <;> simp [Script.compile, validBatch, h, ih]

def Script.forward (script : Script before after) (a : Anchor before.length) :
    Anchor after.length :=
  match script with
  | .done _ => a
  | .next splice valid tail => tail.forward (splice.forward valid a)

def Script.backward (script : Script before after) (a : Anchor after.length) :
    Anchor before.length :=
  match script with
  | .done _ => a
  | .next splice valid tail => splice.backward valid (tail.backward a)

def Script.unaffected (script : Script before after) (a : Anchor before.length) : Prop :=
  match script with
  | .done _ => True
  | .next splice valid tail =>
    (a.offset.val < splice.start ∨ splice.stop < a.offset.val) ∧
      tail.unaffected (splice.forward valid a)

theorem Splice.anchor_roundtrip (splice : Splice) {before : Text}
    (valid : splice.Valid before) (anchor : Anchor before.length)
    (outside : anchor.offset.val < splice.start ∨ splice.stop < anchor.offset.val) :
    splice.backward valid (splice.forward valid anchor) = anchor := by
  cases anchor with
  | mk offset affinity =>
    suffices h : translate splice.start (splice.start + splice.inserted.length)
        (splice.stop - splice.start)
        (translate splice.start splice.stop splice.inserted.length offset.val affinity)
        affinity = offset.val by
      simp [Splice.backward, Splice.forward, h]
    rcases outside with (beforeStart | afterStop)
    · exact translate_prefix_roundtrip beforeStart
    · exact translate_suffix_roundtrip valid.1 afterStop

theorem Script.anchor_roundtrip (script : Script before after) (anchor : Anchor before.length)
    (survives : script.unaffected anchor) :
    script.backward (script.forward anchor) = anchor := by
  induction script with
  | done => rfl
  | next splice valid tail ih =>
    obtain ⟨outside, unaffectedTail⟩ := survives
    simp only [Script.forward, Script.backward]
    rw [ih (splice.forward valid anchor) unaffectedTail]
    exact splice.anchor_roundtrip valid anchor outside

def Script.forwardSelections (script : Script before after) (selections : List (Selection before.length)) :
    List (Selection after.length) :=
  selections.map fun s => ⟨script.forward s.anchor, script.forward s.head⟩

def Script.backwardSelections (script : Script before after) (selections : List (Selection after.length)) :
    List (Selection before.length) :=
  selections.map fun s => ⟨script.backward s.anchor, script.backward s.head⟩

@[simp] theorem Script.forwardSelections_length (script : Script before after)
    (selections : List (Selection before.length)) :
    (script.forwardSelections selections).length = selections.length := by
  simp [Script.forwardSelections]

@[simp] theorem Script.backwardSelections_length (script : Script before after)
    (selections : List (Selection after.length)) :
    (script.backwardSelections selections).length = selections.length := by
  simp [Script.backwardSelections]

end EditorModel
