//! tree-sitter query patterns for symbol extraction per language
//!
//! each query uses capture names like `@definition.function` and `@name`
//! so the symbol extractor can map captures to SymbolKind variants

use crate::language::Language;

/// get the symbol extraction query for a language
pub fn query_for(language: Language) -> &'static str {
    match language {
        Language::Rust => RUST,
        Language::Python => PYTHON,
        Language::JavaScript => JAVASCRIPT,
        Language::TypeScript | Language::Tsx => TYPESCRIPT,
        Language::Go => GO,
        Language::C => C,
        Language::Cpp => CPP,
        Language::Java => JAVA,
        Language::Bash => BASH,
        Language::Nix => NIX,
        // data formats and markup: no meaningful symbols to extract
        Language::Json
        | Language::Toml
        | Language::Yaml
        | Language::Markdown
        | Language::Html
        | Language::Css => "",
    }
}

const RUST: &str = r#"
(function_item
  name: (identifier) @name) @definition.function

(struct_item
  name: (type_identifier) @name) @definition.struct

(enum_item
  name: (type_identifier) @name) @definition.enum

(trait_item
  name: (type_identifier) @name) @definition.trait

(impl_item
  trait: (_)? @_trait
  type: (type_identifier) @name) @definition.impl

(mod_item
  name: (identifier) @name) @definition.module

(const_item
  name: (identifier) @name) @definition.constant

(static_item
  name: (identifier) @name) @definition.constant

(type_item
  name: (type_identifier) @name) @definition.type

(function_signature_item
  name: (identifier) @name) @definition.function

; methods inside impl blocks
(declaration_list
  (function_item
    name: (identifier) @name) @definition.method)
"#;

const PYTHON: &str = r#"
(function_definition
  name: (identifier) @name) @definition.function

(class_definition
  name: (identifier) @name) @definition.class

; methods inside classes
(class_definition
  body: (block
    (function_definition
      name: (identifier) @name) @definition.method))
"#;

const JAVASCRIPT: &str = r#"
(function_declaration
  name: (identifier) @name) @definition.function

(class_declaration
  name: (identifier) @name) @definition.class

(method_definition
  name: (property_identifier) @name) @definition.method

(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function)) @definition.function)

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function)) @definition.function)
"#;

// typescript shares javascript's patterns plus type-specific ones
const TYPESCRIPT: &str = r#"
(function_declaration
  name: (identifier) @name) @definition.function

(class_declaration
  name: (type_identifier) @name) @definition.class

(method_definition
  name: (property_identifier) @name) @definition.method

(interface_declaration
  name: (type_identifier) @name) @definition.interface

(type_alias_declaration
  name: (type_identifier) @name) @definition.type

(enum_declaration
  name: (identifier) @name) @definition.enum

(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function)) @definition.function)
"#;

const GO: &str = r#"
(function_declaration
  name: (identifier) @name) @definition.function

(method_declaration
  name: (field_identifier) @name) @definition.method

(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (struct_type))) @definition.struct

(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (interface_type))) @definition.interface

(const_declaration
  (const_spec
    name: (identifier) @name)) @definition.constant

(var_declaration
  (var_spec
    name: (identifier) @name)) @definition.variable
"#;

const C: &str = r#"
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(struct_specifier
  name: (type_identifier) @name
  body: (_)) @definition.struct

(enum_specifier
  name: (type_identifier) @name
  body: (_)) @definition.enum

(type_definition
  declarator: (type_identifier) @name) @definition.type

(declaration
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function
"#;

const CPP: &str = r#"
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier) @name)) @definition.function

(class_specifier
  name: (type_identifier) @name
  body: (_)) @definition.class

(struct_specifier
  name: (type_identifier) @name
  body: (_)) @definition.struct

(enum_specifier
  name: (type_identifier) @name
  body: (_)) @definition.enum

(namespace_definition
  name: (namespace_identifier) @name) @definition.module
"#;

const JAVA: &str = r#"
(class_declaration
  name: (identifier) @name) @definition.class

(method_declaration
  name: (identifier) @name) @definition.method

(interface_declaration
  name: (identifier) @name) @definition.interface

(enum_declaration
  name: (identifier) @name) @definition.enum

(constructor_declaration
  name: (identifier) @name) @definition.method

(constant_declaration
  declarator: (variable_declarator
    name: (identifier) @name)) @definition.constant
"#;

const BASH: &str = r#"
(function_definition
  name: (word) @name) @definition.function
"#;

const NIX: &str = r#"
(binding
  attrpath: (attrpath
    (identifier) @name)) @definition.variable
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{CODE, DATA};

    #[test]
    fn all_code_languages_have_queries() {
        for lang in CODE {
            assert!(!query_for(*lang).is_empty(), "{lang} has no query");
        }
    }

    #[test]
    fn data_formats_have_no_queries() {
        for lang in DATA {
            assert!(query_for(*lang).is_empty(), "{lang} should have no query");
        }
    }

    #[cfg(feature = "all-languages")]
    #[test]
    fn all_queries_compile() {
        for lang in CODE {
            let ts_lang = lang.tree_sitter_language().unwrap();
            let query_src = query_for(*lang);
            match tree_sitter::Query::new(&ts_lang, query_src) {
                Ok(_) => {}
                Err(e) => panic!("query for {lang} failed to compile: {e}"),
            }
        }
    }
}
