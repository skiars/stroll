import EditorModel

#print axioms EditorModel.Script.compile_iff_valid
#print axioms EditorModel.Script.anchor_roundtrip
#print axioms EditorModel.Buffer.undo_then_redo
#print axioms EditorModel.Buffer.redo_then_undo
#print axioms EditorModel.Patch.resolve_locate
#print axioms EditorModel.Patch.resolve_bounded
#print axioms EditorModel.Workspace.publish_preserves_logical_state
#print axioms EditorModel.Workspace.stale_patch_rejected
#print axioms EditorModel.Workspace.duplicate_patch_preserves_mapping
#print axioms EditorModel.Workspace.step_target
#print axioms EditorModel.Workspace.step_unchanged
#print axioms EditorModel.Workspace.visible_patch_is_current
#print axioms EditorModel.Workspace.run_coherent
#print axioms EditorModel.Utf8.scalar_roundtrip
#print axioms EditorModel.Utf8.byte_roundtrip
#print axioms EditorModel.Utf8.toByte_iff_in_bounds
