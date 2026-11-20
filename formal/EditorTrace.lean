import EditorModel.Utf8
import EditorModel.Workspace
import Lean.Data.Json

open Lean
open EditorModel

namespace EditorTrace

inductive File where
  | first
  | second
  deriving DecidableEq

def parseFile : Nat → Except String File
  | 0 => .ok .first
  | 1 => .ok .second
  | _ => .error "File index must be 0 or 1"

def File.index : File → Nat
  | .first => 0
  | .second => 1

structure ByteSelection where
  anchor : Nat
  head : Nat
  deriving FromJson, ToJson

structure ByteRange where
  start : Nat
  «end» : Nat
  deriving FromJson

structure ByteSplice where
  range : ByteRange
  text : String
  deriving FromJson

def require (option : Option α) (message : String) : Except String α :=
  match option with
  | some value => .ok value
  | none => .error message

def decodeOffset (text : Text) (byte : Nat) : Except String Nat :=
  require (Utf8.toScalar text byte) s!"Invalid UTF-8 byte offset {byte}"

def decodeSelection (text : Text) (selection : ByteSelection) : Except String SelectionRequest := do
  -- The compared views use right affinity for both endpoints.
  return ⟨⟨← decodeOffset text selection.anchor, .right⟩,
    ⟨← decodeOffset text selection.head, .right⟩⟩

def decodeSplices (text : Text) : List ByteSplice → Except String (List Splice)
  | [] => .ok []
  | wire :: rest => do
    let start ← decodeOffset text wire.range.start
    let stop ← decodeOffset text wire.range.end
    if start > stop then
      throw "Reversed splice"
    let splice : Splice := ⟨start, stop, wire.text.toList⟩
    return splice :: (← decodeSplices (splice.apply text) rest)

structure Transaction where
  before : String
  after : String
  deriving ToJson

def undoEntries : Past current → List Transaction
  | .empty _ => []
  | @Past.push before after previous _ =>
    ⟨String.ofList before, String.ofList after⟩ :: undoEntries previous

def redoEntries : Future current → List Transaction
  | .empty _ => []
  | @Future.push before after _ following =>
    ⟨String.ofList before, String.ofList after⟩ :: redoEntries following

structure BufferFrame where
  text : String
  undo : List Transaction
  redo : List Transaction
  deriving ToJson

def bufferFrame (buffer : Buffer) : BufferFrame :=
  ⟨String.ofList buffer.text, undoEntries buffer.past, redoEntries buffer.future⟩

structure Frame where
  buffers : List BufferFrame
  target : Nat
  selections : List ByteSelection
  deriving ToJson

def encodeSelection {text : Text} (selection : Selection text.length) :
    Except String ByteSelection := do
  let anchor ← require (Utf8.toByte text selection.anchor.offset.val) "Invalid model anchor"
  let head ← require (Utf8.toByte text selection.head.offset.val) "Invalid model head"
  return ⟨anchor, head⟩

def frame (state : Workspace File) : Except String Frame := do
  let buffer := state.buffers state.target
  let selections ← buffer.selections.mapM (encodeSelection (text := buffer.text))
  return ⟨[bufferFrame (state.buffers .first), bufferFrame (state.buffers .second)],
    state.target.index, selections⟩

def accept (result : EditorModel.Result File) : Except String (Workspace File) :=
  match result.outcome with
  | .applied | .noChange => .ok result.state
  | other => .error s!"The model rejected the scenario command: {repr other}"

def next (state : Workspace File) (event : Json) : Except String (Workspace File) := do
  match ← event.getObjValAs? String "kind" with
  | "select" =>
    let file ← parseFile (← event.getObjValAs? Nat "file")
    let selections ← event.getObjValAs? (List ByteSelection) "selections"
    let decoded ← selections.mapM (decodeSelection (state.buffers file).text)
    let focused := if file = state.target then state else (state.step (.focus file)).state
    accept (focused.step (.selectSource decoded))
  | "edit" =>
    let file ← parseFile (← event.getObjValAs? Nat "file")
    let splices ← event.getObjValAs? (List ByteSplice) "splices"
    let buffer := state.buffers file
    let decoded ← decodeSplices buffer.text splices
    accept (state.step (.externalEdit file buffer.revision decoded))
  | "input" =>
    let text ← event.getObjValAs? String "text"
    accept (state.step (.input text.toList))
  | "undo" => accept (state.step .undo)
  | "redo" => accept (state.step .redo)
  | "publish" =>
    -- The comparison covers logical state. Git's choice of visible pieces is
    -- observed by the headless driver, while this snapshot uses one full span.
    let length := (state.buffers state.target).text.length
    accept (state.step (.publish ⟨state.ticket, [.source 0 length]⟩))
  | kind => .error s!"Unknown event: {kind}"

def run (request : Json) : Except String (List Frame) := do
  let [first, second] ← request.getObjValAs? (List String) "texts" |
    throw "A trace requires two buffers"
  let events ← request.getObjValAs? (List Json) "events"
  let split ← request.getObjValAs? Bool "split"
  let mut state := Workspace.open (fun file : File =>
    match file with | .first => first.toList | .second => second.toList)
    .first ⟨if split then .split else .unified, 0, 3⟩
  let mut frames := [← frame state]
  for event in events do
    state ← next state event
    frames := (← frame state) :: frames
  return frames.reverse

end EditorTrace

def main : IO UInt32 := do
  let stdin ← IO.getStdin
  let request ← stdin.readToEnd
  match Json.parse request >>= EditorTrace.run with
  | .ok frames =>
    IO.println (toJson frames).compress
    return 0
  | .error message =>
    IO.eprintln message
    return 1
