; Declaration macros such as GX_NODISCARD can make a primitive type appear
; as a recovered identifier instead of a primitive_type.
((ERROR (identifier) @type)
 (#any-of? @type "auto" "bool" "char" "char8_t" "char16_t" "char32_t"
                "double" "float" "int" "long" "short" "signed" "unsigned"
                "void" "wchar_t"))

(operator_name) @function
(operator_name _ @function)
(operator_cast "operator" @keyword)
