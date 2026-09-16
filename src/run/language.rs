//! Built-in ast-grep languages plus the bundled Zig grammar.

use ast_grep_core::matcher::PatternBuilder;
use ast_grep_core::meta_var::MetaVariable;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::{Language, Pattern, PatternError};
use ast_grep_language::SupportLang;
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunLanguage {
    Builtin(SupportLang),
    Zig,
}

impl From<SupportLang> for RunLanguage {
    fn from(language: SupportLang) -> Self {
        Self::Builtin(language)
    }
}

impl std::fmt::Display for RunLanguage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin(lang) => lang.fmt(formatter),
            Self::Zig => formatter.write_str("Zig"),
        }
    }
}

// Zig identifiers are ASCII (or quoted); a legal prefix lets ast-grep parse
// metavariables without broadening the grammar used for real source files.
const META_PREFIX: &str = "__AST_BRO_META_";

impl Language for RunLanguage {
    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        match self {
            Self::Builtin(lang) => lang.pre_process_pattern(query),
            Self::Zig => Cow::Owned(preprocess_zig(query)),
        }
    }

    fn meta_var_char(&self) -> char {
        match self {
            Self::Builtin(lang) => lang.meta_var_char(),
            Self::Zig => '$',
        }
    }

    fn expando_char(&self) -> char {
        match self {
            Self::Builtin(lang) => lang.expando_char(),
            Self::Zig => '$',
        }
    }

    fn extract_meta_var(&self, source: &str) -> Option<MetaVariable> {
        match self {
            Self::Builtin(lang) => lang.extract_meta_var(source),
            // JavaScript uses ast-grep's default `$` metavariable syntax.
            Self::Zig if source.starts_with(META_PREFIX) => {
                SupportLang::JavaScript.extract_meta_var(&source.replace(META_PREFIX, "$"))
            }
            Self::Zig => None,
        }
    }

    fn kind_to_id(&self, kind: &str) -> u16 {
        self.get_ts_language().id_for_node_kind(kind, true)
    }
    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.get_ts_language()
            .field_id_for_name(field)
            .map(|id| id.get())
    }
    fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
        match self {
            Self::Builtin(lang) => lang.build_pattern(builder),
            Self::Zig => builder.build(|source| StrDoc::try_new(source, *self)),
        }
    }
}

fn preprocess_zig(query: &str) -> String {
    let bytes = query.as_bytes();
    let mut output = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        let start = position;
        if bytes[position..].starts_with(b"//") || bytes[position..].starts_with(b"\\\\") {
            while position < bytes.len() && bytes[position] != b'\n' {
                position += 1;
            }
            output.extend_from_slice(&bytes[start..position]);
        } else if matches!(bytes[position], b'"' | b'\'') {
            let delimiter = bytes[position];
            position += 1;
            while position < bytes.len() {
                let ch = bytes[position];
                position += 1;
                if ch == b'\\' {
                    position = (position + 1).min(bytes.len());
                } else if ch == delimiter {
                    break;
                }
            }
            output.extend_from_slice(&bytes[start..position]);
        } else {
            if bytes[position] == b'$' {
                output.extend_from_slice(META_PREFIX.as_bytes());
            } else {
                output.push(bytes[position]);
            }
            position += 1;
        }
    }
    String::from_utf8(output).expect("ASCII substitutions preserve UTF-8")
}

impl LanguageExt for RunLanguage {
    fn get_ts_language(&self) -> TSLanguage {
        match self {
            Self::Builtin(lang) => lang.get_ts_language(),
            Self::Zig => crate::zig_syntax::LANGUAGE.into(),
        }
    }
}
