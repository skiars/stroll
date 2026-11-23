(tag_name) @tag
((tag_name) @type (#match? @type "^[A-Z]"))
(attribute_name) @attribute
(directive_name) @keyword
(directive_value) @attribute
(directive_modifier) @property
(quoted_attribute_value) @string
(comment) @comment
["<" ">" "</" "/>" "{{" "}}" "[" "]"] @punctuation.bracket
["=" ":" "@" "#" "."] @operator
