use eyre::{eyre, Result};
use tracing::debug;

use crate::graph::entities::Language;

/// Parsed syntax tree from tree-sitter.
pub struct ParsedTree {
    pub tree: tree_sitter::Tree,
    pub language: Language,
}

/// AST parser using tree-sitter for multi-language support.
pub struct AstParser {
    parsers: std::collections::HashMap<String, tree_sitter::Parser>,
}

impl AstParser {
    /// Create a new parser with all supported language grammars.
    pub fn new() -> Self {
        let mut parsers = std::collections::HashMap::new();

        // Register each language grammar
        if let Ok(parser) = Self::create_parser(tree_sitter_rust::LANGUAGE.into()) {
            parsers.insert("rust".to_string(), parser);
        }
        if let Ok(parser) = Self::create_parser(tree_sitter_python::LANGUAGE.into()) {
            parsers.insert("python".to_string(), parser);
        }
        if let Ok(parser) = Self::create_parser(tree_sitter_javascript::LANGUAGE.into()) {
            parsers.insert("javascript".to_string(), parser);
        }
        if let Ok(parser) = Self::create_parser(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        {
            parsers.insert("typescript".to_string(), parser);
        }
        if let Ok(parser) = Self::create_parser(tree_sitter_go::LANGUAGE.into()) {
            parsers.insert("go".to_string(), parser);
        }
        if let Ok(parser) = Self::create_parser(tree_sitter_ruby::LANGUAGE.into()) {
            parsers.insert("ruby".to_string(), parser);
        }
        if let Ok(parser) = Self::create_parser(tree_sitter_kotlin_ng::LANGUAGE.into()) {
            parsers.insert("kotlin".to_string(), parser);
        }

        debug!(
            "Initialised tree-sitter parsers for {} languages",
            parsers.len()
        );
        Self { parsers }
    }

    fn create_parser(language: tree_sitter::Language) -> Result<tree_sitter::Parser> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .map_err(|e| eyre!("Failed to set language: {}", e))?;
        Ok(parser)
    }

    /// Parse source code and return a syntax tree.
    pub fn parse(&self, source: &str, language: &Language) -> Result<ParsedTree> {
        let lang_str = language.as_str();
        let parser = self
            .parsers
            .get(lang_str)
            .ok_or_else(|| eyre!("No parser for language: {}", lang_str))?;

        // tree-sitter parsers are not Send, so we need to create a temporary one
        // that uses the same language
        let mut temp_parser = tree_sitter::Parser::new();
        let ts_lang = parser.language().unwrap();
        temp_parser
            .set_language(&ts_lang)
            .map_err(|e| eyre!("Failed to set language: {}", e))?;

        let tree = temp_parser
            .parse(source.as_bytes(), None)
            .ok_or_else(|| eyre!("Failed to parse source"))?;

        Ok(ParsedTree {
            tree,
            language: language.clone(),
        })
    }

    /// Check if a language is supported.
    pub fn supports_language(&self, language: &Language) -> bool {
        self.parsers.contains_key(language.as_str())
    }
}

impl Default for AstParser {
    fn default() -> Self {
        Self::new()
    }
}
