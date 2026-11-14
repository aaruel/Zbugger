use super::{Language, SyntaxRules, TreeSitterGrammar};

/// C and C++ share these rules; `.c` and `.h` files are treated as C++.
pub static CPP: Language = Language {
    name: "C++",
    extensions: &[
        "cpp", "cc", "cxx", "c++", "cp", "c", "h", "hpp", "hh", "hxx", "h++", "inl", "ipp", "tpp", "ixx", "cppm",
    ],
    rules: SyntaxRules {
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        string_delims: b"\"'",
        escape: b'\\',
        string_prefixes: &["L", "u", "U", "u8"],
        raw_string_prefixes: &["R", "LR", "uR", "UR", "u8R"],
        preprocessor: Some(b'#'),
        digit_separators: true,
        highlight_calls: true,
        keywords: KEYWORDS,
        types: TYPES,
    },
    tree_sitter: Some(TreeSitterGrammar::new(
        "cpp",
        || tree_sitter_cpp::LANGUAGE.into(),
        include_str!("queries/cpp_highlights.scm"),
    )),
};

const KEYWORDS: &[&str] = &[
    "alignas", "alignof", "and", "and_eq", "asm", "auto", "bitand", "bitor", "break", "case", "catch",
    "class", "co_await", "co_return", "co_yield", "compl", "concept", "const", "const_cast", "consteval",
    "constexpr", "constinit", "continue", "decltype", "default", "delete", "do", "dynamic_cast", "else",
    "enum", "explicit", "export", "extern", "false", "final", "for", "friend", "goto", "if", "import",
    "inline", "module", "mutable", "namespace", "new", "noexcept", "not", "not_eq", "nullptr", "operator",
    "or", "or_eq", "override", "private", "protected", "public", "register", "reinterpret_cast", "requires",
    "restrict", "return", "sizeof", "static", "static_assert", "static_cast", "struct", "switch", "template",
    "this", "thread_local", "throw", "true", "try", "typedef", "typeid", "typename", "union", "using",
    "virtual", "volatile", "while", "xor", "xor_eq", "NULL",
];

const TYPES: &[&str] = &[
    "bool", "char", "char8_t", "char16_t", "char32_t", "double", "float", "int", "long", "short", "signed",
    "unsigned", "void", "wchar_t", "size_t", "ssize_t", "ptrdiff_t", "intptr_t", "uintptr_t", "intmax_t",
    "uintmax_t", "nullptr_t", "max_align_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t",
    "uint16_t", "uint32_t", "uint64_t",
];
