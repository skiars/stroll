import EditorModel.Document

namespace EditorModel.Utf8

/-- Convert a scalar boundary to a UTF-8 byte boundary. EOF is a boundary. -/
def toByte : Text → Nat → Option Nat
  | _, 0 => some 0
  | [], _ + 1 => none
  | char :: rest, offset + 1 => (toByte rest offset).map (char.utf8Size + ·)

/-- Reject out-of-bounds offsets and offsets inside a UTF-8 sequence. -/
def toScalar : Text → Nat → Option Nat
  | _, 0 => some 0
  | [], _ + 1 => none
  | char :: rest, offset + 1 =>
    if char.utf8Size ≤ offset + 1 then
      (toScalar rest (offset + 1 - char.utf8Size)).map (· + 1)
    else none

theorem toByte_iff_in_bounds (text : Text) (scalar : Nat) :
    (toByte text scalar).isSome = true ↔ scalar ≤ text.length := by
  induction text generalizing scalar with
  | nil => cases scalar <;> simp [toByte]
  | cons char rest ih => cases scalar <;> simp [toByte, ih]

theorem scalar_roundtrip (text : Text) (scalar byte : Nat)
    (converted : toByte text scalar = some byte) :
    toScalar text byte = some scalar := by
  induction text generalizing scalar byte with
  | nil =>
    cases scalar with
    | zero => simp [toByte] at converted; subst byte; rfl
    | succ => simp [toByte] at converted
  | cons char rest ih =>
    cases scalar with
    | zero => simp [toByte] at converted; subst byte; rfl
    | succ offset =>
      cases suffix : toByte rest offset with
      | none => simp [toByte, suffix] at converted
      | some inner =>
        simp [toByte, suffix] at converted
        subst byte
        have positive := char.utf8Size_pos
        cases sum : char.utf8Size + inner with
        | zero => omega
        | succ remaining =>
          have enough : char.utf8Size ≤ remaining + 1 := by omega
          have left : remaining + 1 - char.utf8Size = inner := by omega
          have tail := ih offset inner suffix
          simp [toScalar, enough, left, tail]

theorem byte_roundtrip (text : Text) (byte scalar : Nat)
    (converted : toScalar text byte = some scalar) :
    toByte text scalar = some byte := by
  induction text generalizing byte scalar with
  | nil =>
    cases byte with
    | zero => simp [toScalar] at converted; subst scalar; rfl
    | succ => simp [toScalar] at converted
  | cons char rest ih =>
    cases byte with
    | zero => simp [toScalar] at converted; subst scalar; rfl
    | succ offset =>
      by_cases enough : char.utf8Size ≤ offset + 1
      · cases tail : toScalar rest (offset + 1 - char.utf8Size) with
        | none => simp [toScalar, enough, tail] at converted
        | some inner =>
          simp [toScalar, enough, tail] at converted
          subst scalar
          have joined : char.utf8Size + (offset + 1 - char.utf8Size) = offset + 1 := by omega
          simp [toByte, ih _ _ tail, joined]
      · simp [toScalar, enough] at converted

example : toByte "aé文🙂".toList 4 = some 10 := by decide +kernel
example : toScalar "aé文🙂".toList 6 = some 3 := by decide +kernel
example : toScalar "aé文🙂".toList 5 = none := by decide +kernel
example : toScalar "aé文🙂".toList 11 = none := by decide +kernel

end EditorModel.Utf8
