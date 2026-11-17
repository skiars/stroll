((code_span) @text.code.span
  (#set! highlight.allow-overlap))

(emphasis_delimiter) @punctuation.delimiter
(code_span_delimiter) @punctuation.delimiter.code

((emphasis) @emphasis
  (#set! highlight.allow-overlap))

((strong_emphasis) @emphasis.strong
  (#set! highlight.allow-overlap))

[
  (link_destination)
  (uri_autolink)
] @link_uri

[
  (link_label)
  (link_text)
  (image_description)
] @link_text
