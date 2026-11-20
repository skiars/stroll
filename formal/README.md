# Editor semantics in Lean 4

This standalone Lake package uses the toolchain in `lean-toolchain` and has no
external package dependencies. From the repository root, run:

```sh
cd formal
lake build
lake env lean CheckProofs.lean
lake env leanchecker EditorModel
```

The build elaborates the model, proofs, and executable examples. Warnings,
including incomplete proofs, fail the build. The audit prints the axioms used
by the main theorems.
The checker replays the package's compiled declarations through the kernel.

## Model

The model describes one editing session over a fixed collection of buffers,
with arbitrary text lengths, selection counts, and command sequences.

- [Document](EditorModel/Document.lean) uses Unicode scalar boundaries. A
  selection stores an oriented anchor and head with insertion affinity. A batch
  of source splices uses sequential coordinates and validates against the
  intermediate text. Compilation either produces a typed edit script or fails
  atomically.
- [Buffer](EditorModel/Buffer.lean) maintains compatible undo and redo stacks.
  Input replaces selections from right to left and collapses each to a
  right-affine caret. Ambiguous, overlapping input ranges are rejected. Undo
  and redo transform the session's current selections. The model preserves
  their order and count, including when several positions converge.
- [Workspace](EditorModel/Workspace.lean) gives each command an explicit
  outcome. Focus chooses the history target. Logical positions remain valid
  when omitted from the projection. Two visible selection endpoints define a
  source range, which can include omitted text between them. Projection pieces
  refer to checked source intervals or carry read-only decorations. A patch
  ticket includes the file, source revision, and view generation. A stale
  result is rejected. A current installed mapping is immutable for its ticket.
- [Invariants](EditorModel/Invariants.lean) proves that rejected and no-op
  commands preserve state, published patches are current, successful history
  round trips restore text and both stacks, and revisions are coherent across
  arbitrary command sequences. The model also proves that locating a visible
  anchor and resolving it returns the same position, and that an anchor
  strictly outside each splice round-trips through the script and its inverse.
- [Examples](EditorModel/Examples.lean) checks the intended outcomes for the
  three Git review counterexamples, including typing after undo, redoing an
  edit whose visible run disappeared, and preserving multiple carets through
  patch publication. It also checks rejection of invalid batches and stale
  results.

## Run the Rust and Lean comparison

From the repository root:

```sh
(cd formal && lake build editor_trace)
env -u DISPLAY -u WAYLAND_DISPLAY cargo test --release --locked --lib \
  lean_headless_differential -- --ignored
```

Set `STROLL_LEAN_TRACE` to use an executable from another build directory.
The default Cargo suite runs the headless fixtures with the Rust reference
model. The opt-in test runs their same traces through the Lean executable.
Each subprocess consumes one JSON trace on stdin and returns a JSON array
containing the initial frame and a frame after each command.

- [Utf8](EditorModel/Utf8.lean) proves both directions of the conversion
  between scalar boundaries and UTF-8 byte boundaries. Invalid byte offsets
  are rejected, including offsets inside a code point. Each splice is decoded
  against the text produced by the preceding splice.
- The bridge compares the primary anchor and head in order, and secondary
  collapsed carets by position. Both Lean endpoints use right affinity in
  these fixtures. Selection and history-target comparisons begin at the
  first explicit selection. For folded selections the bridge reads the
  saved source endpoints. The test also checks that folding removes the
  visible mapping and unfolding restores it.
- The bridge compares the current text and the before/after text of every
  entry in each file's undo and redo stacks, starting at the top. A fixture
  supplies the exact semantic splices to Lean and submits one whole-buffer
  replacement to the Rust source editor. The shared transaction boundary
  and resulting text must match. The recorded editor history is decoded
  using its actual byte ranges. This permits different splice granularity
  inside a transaction.
- The three regressions compare every frame for undo across folded context,
  redo when a displayed run disappears, and multiple carets through a patch
  refresh. The test also compares the bounded enumeration, explicit file
  focus switches, and a multilingual trace with a reversed primary
  selection and a secondary caret.

The proofs apply to the specification. The
[headless comparison](../src/workspace/git_panel/tests/edit_model_tests.rs)
checks finite implementation traces. Publication compares the logical state
on both sides. Rust supplies the real Git projection, while the bridge uses a
full source span in Lean. The scope remains in-memory, single-buffer
transactions. Input method composition, typing coalescence, cross-file
history groups, persistence, and how Git chooses display pieces are outside
this comparison.
