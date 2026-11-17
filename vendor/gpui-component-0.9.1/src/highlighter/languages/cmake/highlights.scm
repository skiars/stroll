; Use predicates supported by Tree-sitter's Rust QueryCursor.
((unquoted_argument) @constant
  (#match? @constant "^[A-Z_][A-Z0-9_]*$"))

[
  (quoted_argument)
  (bracket_argument)
] @string

[
  (line_comment)
  (bracket_comment)
] @comment

(variable) @variable
(escape_sequence) @string.escape

(normal_command (identifier) @function)
(function_command (argument_list . (argument) @function))
(macro_command (argument_list . (argument) @function))

[
  (if)
  (elseif)
  (else)
  (endif)
  (foreach)
  (endforeach)
  (while)
  (endwhile)
  (function)
  (endfunction)
  (macro)
  (endmacro)
  (block)
  (endblock)
] @keyword

[
  "("
  ")"
] @punctuation.bracket

[
  "$"
  "{"
  "}"
] @punctuation.special
