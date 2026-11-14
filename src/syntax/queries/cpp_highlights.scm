; Zbugger's C++ highlight query for tree-sitter-cpp.
;
; When several patterns capture the same node, the LAST pattern wins,
; so general patterns come first and more specific ones after them.
; Capture names map to colors in src/syntax/ts_highlight.rs.

; ---- Operators and punctuation --------------------------------------------

[
  "--" "-" "-=" "->" "->*" "=" "!=" "!" "*" "*=" "&" "&&" "&=" "+" "++" "+="
  "<" "<<" "<<=" "<=" "<=>" "==" ">" ">=" ">>" ">>=" "||" "|" "|=" "^" "^="
  "%" "%=" "/" "/=" "~" ".*" "..." "?"
] @operator

["(" ")" "[" "]" "{" "}" ";" "," "." ":" "::"] @punctuation

; ---- Keywords -------------------------------------------------------------

[
  "alignas" "alignof" "and" "and_eq" "asm" "bitand" "bitor" "break" "case"
  "catch" "class" "co_await" "co_return" "co_yield" "compl" "concept" "const"
  "consteval" "constexpr" "constinit" "continue" "decltype" "default" "delete"
  "do" "else" "enum" "explicit" "extern" "final" "for" "friend" "goto" "if"
  "inline" "mutable" "namespace" "new" "noexcept" "noreturn" "not" "not_eq"
  "offsetof" "operator" "or" "or_eq" "override" "private" "protected" "public"
  "register" "requires" "restrict" "return" "sizeof" "static" "static_assert"
  "struct" "switch" "template" "thread_local" "throw" "try" "typedef"
  "typename" "union" "using" "virtual" "volatile" "while" "xor" "xor_eq"
] @keyword

(this) @keyword

; ---- Names ----------------------------------------------------------------

; ALL_CAPS identifiers are usually macros or constants.
((identifier) @constant
 (#match? @constant "^[A-Z][A-Z0-9_]+$"))

(field_identifier) @property

; ---- Types and namespaces -------------------------------------------------

[
  (primitive_type)
  (sized_type_specifier)
  (type_identifier)
  (auto)
] @type

(namespace_identifier) @namespace

; ---- Functions: declarations and calls ------------------------------------

(function_declarator declarator: (identifier) @function)
(function_declarator declarator: (field_identifier) @function)
(function_declarator declarator: (destructor_name) @function)
(function_declarator declarator: (operator_name) @function)
(function_declarator
  declarator: (qualified_identifier name: (identifier) @function))
(function_declarator
  declarator: (qualified_identifier
    name: (qualified_identifier name: (identifier) @function)))
(function_declarator
  declarator: (qualified_identifier name: (destructor_name) @function))

(call_expression function: (identifier) @function)
(call_expression
  function: (qualified_identifier name: (identifier) @function))
(call_expression
  function: (qualified_identifier
    name: (qualified_identifier name: (identifier) @function)))
(call_expression
  function: (field_expression field: (field_identifier) @function))
(template_function name: (identifier) @function)
(template_method name: (field_identifier) @function)

; Casts read as keywords, not function calls.
((template_function name: (identifier) @keyword)
 (#match? @keyword "^(static|dynamic|const|reinterpret)_cast$"))

; ---- Literals -------------------------------------------------------------

(number_literal) @number
(literal_suffix) @number

[(true) (false) (null)] @constant

[
  (string_literal)
  (raw_string_literal)
  (char_literal)
] @string

; ---- Preprocessor ---------------------------------------------------------

[
  "#define" "#elif" "#elifdef" "#elifndef" "#else" "#endif"
  "#if" "#ifdef" "#ifndef" "#include"
] @preprocessor
(preproc_directive) @preprocessor
"defined" @preprocessor

(preproc_def name: (identifier) @constant)
(preproc_function_def name: (identifier) @function)
(system_lib_string) @string

; ---- Comments -------------------------------------------------------------

(comment) @comment
