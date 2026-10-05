; Kotlin highlights for tree-sitter-kotlin-ng, which ships no queries. Patterns earlier in the
; file win over later ones for the same node.

(line_comment) @comment
(block_comment) @comment
(shebang) @comment

(string_literal) @string
(multiline_string_literal) @string
(character_literal) @string
(escape_sequence) @string.escape
(interpolation ["$" "${" "}"] @punctuation.special)

(number_literal) @number
(float_literal) @number

((identifier) @keyword.control
 (#any-of? @keyword.control "break" "continue"))

((identifier) @constant.builtin
 (#any-of? @constant.builtin "true" "false" "null"))

(function_declaration name: (identifier) @function)
(call_expression (identifier) @function.call)
(call_expression (navigation_expression (identifier) @function.method.call .))

(class_declaration name: (identifier) @type)
(object_declaration name: (identifier) @type)
(type_alias type: (identifier) @type)
(user_type (identifier) @type)
(enum_entry (identifier) @constant)

(annotation "@" @attribute (user_type (identifier) @attribute))
(annotation "@" @attribute (constructor_invocation (user_type (identifier) @attribute)))
(file_annotation) @attribute
(reification_modifier) @keyword

(label) @label
(package_header (qualified_identifier) @module)
(import (qualified_identifier) @module)

(parameter (identifier) @variable.parameter)
(class_parameter (identifier) @variable.parameter)
(navigation_expression (identifier) @property .)

(this_expression) @variable.builtin
(super_expression) @variable.builtin

["if" "else" "when" "for" "while" "do" "try" "catch" "finally" "throw" "return" "return@"] @keyword.control
["import" "package"] @keyword.import

[
  "class" "interface" "object" "fun" "val" "var" "typealias" "constructor" "init" "companion"
  "by" "where" "get" "set" "in" "is" "as" "as?" "!in" "!is" "this@" "super@"
  "abstract" "actual" "annotation" "const" "crossinline" "data" "enum" "expect" "external"
  "final" "infix" "inline" "inner" "internal" "lateinit" "noinline" "open" "operator" "out"
  "override" "private" "protected" "public" "sealed" "suspend" "tailrec" "value" "vararg"
] @keyword

[
  "+" "-" "*" "/" "%" "=" "+=" "-=" "*=" "/=" "%=" "==" "!=" "===" "!==" "<" ">" "<=" ">="
  "&&" "||" "!" "!!" "++" "--" "?:" "?." "::" ".." "..<" "->"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ";" ":"] @punctuation.delimiter

(identifier) @variable
