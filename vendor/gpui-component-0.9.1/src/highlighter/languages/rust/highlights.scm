; Identifiers

(type_identifier) @type
(primitive_type) @type.builtin
(field_identifier) @property

; Identifier conventions

; Assume all-caps names are constants
((identifier) @constant
 (#match? @constant "^[A-Z][A-Z\\d_]+$"))

; Assume uppercase names are enum constructors
((identifier) @constructor
 (#match? @constructor "^[A-Z]"))

; Assume that uppercase names in paths are types
((scoped_identifier
  path: (identifier) @type)
 (#match? @type "^[A-Z]"))
((scoped_identifier
  path: (scoped_identifier
    name: (identifier) @type))
 (#match? @type "^[A-Z]"))
((scoped_type_identifier
  path: (identifier) @type)
 (#match? @type "^[A-Z]"))
((scoped_type_identifier
  path: (scoped_identifier
    name: (identifier) @type))
 (#match? @type "^[A-Z]"))

; Assume all qualified names in struct patterns are enum constructors. (They're
; either that, or struct names; highlighting both as constructors seems to be
; the less glaring choice of error, visually.)
(struct_pattern
  type: (scoped_type_identifier
    name: (type_identifier) @constructor))

; Function calls

(call_expression
  function: (identifier) @function)
(call_expression
  function: (field_expression
    field: (field_identifier) @function.method))
(call_expression
  function: (scoped_identifier
    "::"
    name: (identifier) @function))

(generic_function
  function: (identifier) @function)
(generic_function
  function: (scoped_identifier
    name: (identifier) @function))
(generic_function
  function: (field_expression
    field: (field_identifier) @function.method))

(macro_invocation
  macro: (identifier) @function.macro
  "!" @function.macro)

; Function definitions

(function_item (identifier) @function)
(function_signature_item (identifier) @function)

[
  (line_comment)
  (block_comment)
] @comment

[
  (line_comment (doc_comment))
  (block_comment (doc_comment))
] @comment.doc

"(" @punctuation.bracket
")" @punctuation.bracket
"[" @punctuation.bracket
"]" @punctuation.bracket
"{" @punctuation.bracket
"}" @punctuation.bracket

(type_arguments
  "<" @punctuation.bracket
  ">" @punctuation.bracket)
(type_parameters
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

"::" @punctuation.delimiter
":" @punctuation.delimiter
"." @punctuation.delimiter
"," @punctuation.delimiter
";" @punctuation.delimiter

(parameter (identifier) @variable.parameter)

(lifetime (identifier) @label)
(type_parameter name: (type_identifier) @type.parameter)
(trait_item name: (type_identifier) @type.interface)
(mod_item name: (identifier) @namespace)

"as" @keyword
"async" @keyword.modifier
"await" @keyword.modifier
"break" @keyword.return
"const" @keyword.modifier
"continue" @keyword.return
"default" @keyword.modifier
"dyn" @keyword.modifier
"else" @keyword.conditional
"enum" @keyword
"extern" @keyword.modifier
"fn" @keyword
"for" @keyword.repeat
"gen" @keyword
"if" @keyword.conditional
"impl" @keyword
"in" @keyword.repeat
"let" @keyword
"loop" @keyword.repeat
"macro_rules!" @keyword
"match" @keyword.conditional
"mod" @keyword
"move" @keyword.modifier
"pub" @keyword.modifier
"raw" @keyword.modifier
"ref" @keyword.modifier
"return" @keyword.return
"static" @keyword.modifier
"struct" @keyword
"trait" @keyword
"type" @keyword
"union" @keyword
"unsafe" @keyword.modifier
"use" @keyword
"where" @keyword
"while" @keyword.repeat
"yield" @keyword.return
(crate) @keyword
(mutable_specifier) @keyword.modifier
(use_list (self) @keyword)
(scoped_use_list (self) @keyword)
(scoped_identifier (self) @keyword)
(super) @keyword

(self) @variable.builtin

(char_literal) @string
(string_literal) @string
(raw_string_literal) @string

(boolean_literal) @constant.builtin.boolean
(integer_literal) @constant.builtin.number
(float_literal) @constant.builtin.number

(escape_sequence) @escape

(attribute_item) @attribute
(inner_attribute_item) @attribute

"*" @operator
"&" @operator
"'" @operator