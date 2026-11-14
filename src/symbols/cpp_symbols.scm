; Symbols for Zbugger's go-to-definition index (tree-sitter-cpp).
;
; Each pattern captures the symbol's name as @name, and the whole construct
; as @definition.<kind> or @declaration.<kind>. The kinds are mapped in
; src/symbols/extract.rs.

; ---- Functions and methods ---------------------------------------------------

(function_definition
  declarator: (function_declarator declarator: (_) @name)) @definition.function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (_) @name))) @definition.function
(function_definition
  declarator: (reference_declarator
    (function_declarator declarator: (_) @name))) @definition.function

; Prototypes, at namespace scope and inside classes.
(declaration
  declarator: (function_declarator declarator: (_) @name)) @declaration.function
(declaration
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (_) @name))) @declaration.function
(declaration
  declarator: (reference_declarator
    (function_declarator declarator: (_) @name))) @declaration.function
(field_declaration
  declarator: (function_declarator declarator: (_) @name)) @declaration.function
(field_declaration
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (_) @name))) @declaration.function
(field_declaration
  declarator: (reference_declarator
    (function_declarator declarator: (_) @name))) @declaration.function

; ---- Types --------------------------------------------------------------------

; Only with a body; `class Foo;` forward declarations are skipped.
(class_specifier name: (_) @name body: (_)) @definition.class
(struct_specifier name: (_) @name body: (_)) @definition.struct
(union_specifier name: (_) @name body: (_)) @definition.union
(enum_specifier name: (_) @name body: (_)) @definition.enum
(enumerator name: (identifier) @name) @definition.enumerator

(type_definition declarator: (type_identifier) @name) @definition.typedef
(alias_declaration name: (type_identifier) @name) @definition.typedef

(namespace_definition name: (_) @name) @definition.namespace

; ---- Members and globals ---------------------------------------------------

(field_declaration declarator: (field_identifier) @name) @definition.field
(field_declaration
  declarator: (pointer_declarator declarator: (field_identifier) @name)) @definition.field

; Variables at file or namespace scope (not locals inside functions).
(translation_unit
  (declaration declarator: (init_declarator declarator: (identifier) @name)) @definition.variable)
(translation_unit
  (declaration declarator: (identifier) @name) @definition.variable)
(declaration_list
  (declaration declarator: (init_declarator declarator: (identifier) @name)) @definition.variable)
(declaration_list
  (declaration declarator: (identifier) @name) @definition.variable)

; ---- Macros -------------------------------------------------------------------

(preproc_def name: (identifier) @name) @definition.macro
(preproc_function_def name: (identifier) @name) @definition.macro
