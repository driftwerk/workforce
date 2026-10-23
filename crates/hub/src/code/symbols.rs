//! code.symbols - Language-aware symbol extraction (functions, structs, classes).

use async_trait::async_trait;
use eyre::{eyre, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::validate_path;

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

/// code.symbols tool - Extract symbols from source files.
pub struct CodeSymbolsTool {
    allowed_paths: Vec<PathBuf>,
}

impl CodeSymbolsTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    /// Extract symbols from file based on language.
    fn extract_symbols(
        &self,
        path: &std::path::Path,
        symbol_type_filter: Option<&str>,
    ) -> Result<SymbolsOutput> {
        let validated_path = validate_path(path, &self.allowed_paths)?;

        // Check file size
        let metadata = std::fs::metadata(&validated_path)?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err(eyre!(
                "File too large: {} bytes (max: {} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            ));
        }

        // Read file content
        let content = std::fs::read_to_string(&validated_path)?;

        // Detect language from extension
        let extension = validated_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let language = match extension {
            "rs" => "rust",
            "ts" | "tsx" => "typescript",
            "js" | "jsx" => "javascript",
            "py" => "python",
            "rb" => "ruby",
            "go" => "go",
            _ => "unknown",
        };

        // Extract symbols based on language
        let symbols = match language {
            "rust" => self.extract_rust_symbols(&content, symbol_type_filter)?,
            "typescript" | "javascript" => {
                self.extract_typescript_symbols(&content, symbol_type_filter)?
            }
            "python" => self.extract_python_symbols(&content, symbol_type_filter)?,
            "ruby" => self.extract_ruby_symbols(&content, symbol_type_filter)?,
            "go" => self.extract_go_symbols(&content, symbol_type_filter)?,
            _ => Vec::new(),
        };

        Ok(SymbolsOutput {
            path: validated_path.display().to_string(),
            language: language.to_string(),
            symbols,
        })
    }

    /// Extract Rust symbols (fn, struct, impl, trait, enum).
    fn extract_rust_symbols(&self, content: &str, filter: Option<&str>) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();

        // Match functions: pub fn name(...) or fn name(...)
        if filter.is_none() || filter == Some("function") {
            let fn_regex = Regex::new(r"(?m)^\s*(pub\s+)?fn\s+(\w+)\s*(<[^>]*>)?\s*\((.*?)\)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = fn_regex.captures(line) {
                    let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    let params = caps.get(4).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "function".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("fn {}({})", name, params),
                    });
                }
            }
        }

        // Match structs: pub struct Name or struct Name
        if filter.is_none() || filter == Some("struct") {
            let struct_regex = Regex::new(r"(?m)^\s*(pub\s+)?struct\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = struct_regex.captures(line) {
                    let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "struct".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("struct {}", name),
                    });
                }
            }
        }

        // Match traits: pub trait Name or trait Name
        if filter.is_none() || filter == Some("trait") {
            let trait_regex = Regex::new(r"(?m)^\s*(pub\s+)?trait\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = trait_regex.captures(line) {
                    let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "trait".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("trait {}", name),
                    });
                }
            }
        }

        // Match enums: pub enum Name or enum Name
        if filter.is_none() || filter == Some("enum") {
            let enum_regex = Regex::new(r"(?m)^\s*(pub\s+)?enum\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = enum_regex.captures(line) {
                    let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "enum".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("enum {}", name),
                    });
                }
            }
        }

        Ok(symbols)
    }

    /// Extract TypeScript/JavaScript symbols (function, class, interface).
    fn extract_typescript_symbols(
        &self,
        content: &str,
        filter: Option<&str>,
    ) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();

        // Match functions: function name(...) or export function name(...)
        if filter.is_none() || filter == Some("function") {
            let fn_regex =
                Regex::new(r"(?m)^\s*(export\s+)?(async\s+)?function\s+(\w+)\s*\((.*?)\)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = fn_regex.captures(line) {
                    let name = caps.get(3).map(|m| m.as_str()).unwrap_or("");
                    let params = caps.get(4).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "function".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("function {}({})", name, params),
                    });
                }
            }

            // Match arrow functions: const name = (...) =>
            let arrow_regex =
                Regex::new(r"(?m)^\s*(?:export\s+)?const\s+(\w+)\s*=\s*\((.*?)\)\s*=>")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = arrow_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let params = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "function".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("const {} = ({})", name, params),
                    });
                }
            }
        }

        // Match classes: class Name or export class Name
        if filter.is_none() || filter == Some("class") {
            let class_regex = Regex::new(r"(?m)^\s*(export\s+)?class\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = class_regex.captures(line) {
                    let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "class".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("class {}", name),
                    });
                }
            }
        }

        // Match interfaces: interface Name or export interface Name
        if filter.is_none() || filter == Some("interface") {
            let interface_regex = Regex::new(r"(?m)^\s*(export\s+)?interface\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = interface_regex.captures(line) {
                    let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "interface".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("interface {}", name),
                    });
                }
            }
        }

        Ok(symbols)
    }

    /// Extract Python symbols (def, class).
    fn extract_python_symbols(&self, content: &str, filter: Option<&str>) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();

        // Match functions: def name(...):
        if filter.is_none() || filter == Some("function") {
            let fn_regex = Regex::new(r"(?m)^\s*def\s+(\w+)\s*\((.*?)\)\s*:")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = fn_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let params = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "function".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("def {}({})", name, params),
                    });
                }
            }
        }

        // Match classes: class Name:
        if filter.is_none() || filter == Some("class") {
            let class_regex = Regex::new(r"(?m)^\s*class\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = class_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "class".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("class {}", name),
                    });
                }
            }
        }

        Ok(symbols)
    }

    /// Extract Ruby symbols (def, class, module).
    fn extract_ruby_symbols(&self, content: &str, filter: Option<&str>) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();

        // Match methods: def name
        if filter.is_none() || filter == Some("function") {
            let fn_regex = Regex::new(r"(?m)^\s*def\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = fn_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "function".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("def {}", name),
                    });
                }
            }
        }

        // Match classes: class Name
        if filter.is_none() || filter == Some("class") {
            let class_regex = Regex::new(r"(?m)^\s*class\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = class_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "class".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("class {}", name),
                    });
                }
            }
        }

        // Match modules: module Name
        if filter.is_none() || filter == Some("module") {
            let module_regex = Regex::new(r"(?m)^\s*module\s+(\w+)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = module_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "module".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("module {}", name),
                    });
                }
            }
        }

        Ok(symbols)
    }

    /// Extract Go symbols (func, struct, interface).
    fn extract_go_symbols(&self, content: &str, filter: Option<&str>) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();

        // Match functions: func name(...) or func (receiver) name(...)
        if filter.is_none() || filter == Some("function") {
            let fn_regex = Regex::new(r"(?m)^\s*func\s+(?:\([^)]*\)\s+)?(\w+)\s*\((.*?)\)")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = fn_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let params = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "function".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("func {}({})", name, params),
                    });
                }
            }
        }

        // Match structs: type Name struct
        if filter.is_none() || filter == Some("struct") {
            let struct_regex = Regex::new(r"(?m)^\s*type\s+(\w+)\s+struct")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = struct_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "struct".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("type {} struct", name),
                    });
                }
            }
        }

        // Match interfaces: type Name interface
        if filter.is_none() || filter == Some("interface") {
            let interface_regex = Regex::new(r"(?m)^\s*type\s+(\w+)\s+interface")?;
            for (line_num, line) in content.lines().enumerate() {
                if let Some(caps) = interface_regex.captures(line) {
                    let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    symbols.push(Symbol {
                        symbol_type: "interface".to_string(),
                        name: name.to_string(),
                        line: line_num + 1,
                        signature: format!("type {} interface", name),
                    });
                }
            }
        }

        Ok(symbols)
    }
}

#[derive(Deserialize)]
struct CodeSymbolsInput {
    path: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    symbol_type: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Symbol {
    #[serde(rename = "type")]
    symbol_type: String,
    name: String,
    line: usize,
    signature: String,
}

#[derive(Serialize, Deserialize)]
struct SymbolsOutput {
    path: String,
    language: String,
    symbols: Vec<Symbol>,
}

/// `path` inside the checkout `repo` resolved to, `root`: a path given with a
/// repo names a file of that repo, wherever `..`, an absolute path or a link
/// along it would lead.
fn within_repo(root: &Path, repo: &str, path: &str) -> Result<PathBuf> {
    workforce_agent::tools::paths::CanonicalRoots::new(&[root.to_path_buf()])
        .resolve(&root.join(path))
        .ok_or_else(|| eyre!("'{path}' is not a file of repository '{repo}'"))
}

#[async_trait]
impl Tool for CodeSymbolsTool {
    fn name(&self) -> &str {
        "code.symbols"
    }

    fn description(&self) -> &str {
        "Extract language-aware symbols (functions, structs, classes, interfaces) from"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to extract symbols from"
                },
                "symbol_type": {
                    "type": "string",
                    "enum": ["function", "struct", "class", "interface", "trait", "enum", "module"],
                    "description": "Filter by symbol type (optional, default: all)"
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeSymbolsInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        let path = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base)
                .and_then(|root| within_repo(&root, repo, &input.path))
            {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Symbol extraction failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            PathBuf::from(&input.path)
        };

        match self.extract_symbols(&path, input.symbol_type.as_deref()) {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Symbol extraction failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_rust_function_extraction() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.rs");
        fs::write(
            &test_file,
            "fn main() {}\npub fn helper(x: i32) -> i32 { x }\n",
        )
        .unwrap();

        let tool = CodeSymbolsTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.rs"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: SymbolsOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.language, "rust");
        assert_eq!(output.symbols.len(), 2);
        assert_eq!(output.symbols[0].name, "main");
        assert_eq!(output.symbols[1].name, "helper");
    }

    /// With `repo`, the path names a file of that checkout: `..`, an
    /// absolute path or a link out of it is refused, though each lands inside
    /// the workspace the tool may otherwise read.
    #[tokio::test]
    async fn a_path_given_with_a_repo_stays_inside_that_repo() {
        let temp = TempDir::new().unwrap();
        let ws = temp.path().canonicalize().unwrap();
        let repo = ws.join("repos/acme/app");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join("lib.rs"), "pub fn inside() {}\n").unwrap();
        fs::write(ws.join("outside.rs"), "pub fn outside() {}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(ws.join("outside.rs"), repo.join("link.rs")).unwrap();
        let tool = CodeSymbolsTool::new(vec![ws.clone()]);
        let symbols = |path: String| {
            let tool = &tool;
            async move {
                tool.execute(serde_json::json!({ "repo": "acme/app", "path": path }))
                    .await
                    .unwrap()
            }
        };

        let inside = symbols("lib.rs".into()).await;
        assert!(inside.success, "{inside:?}");
        let mut escapes = vec![
            "../../../outside.rs".to_string(),
            ws.join("outside.rs").display().to_string(),
        ];
        if cfg!(unix) {
            escapes.push("link.rs".into());
        }
        for path in escapes {
            let refused = symbols(path.clone()).await;
            assert!(!refused.success, "{path}");
            assert!(
                refused
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("is not a file of repository 'acme/app'")),
                "{path}: {refused:?}"
            );
        }
        assert!(
            tool.execute(serde_json::json!({ "path": "outside.rs" }))
                .await
                .unwrap()
                .success,
            "without a repo the workspace is the root"
        );
    }

    #[tokio::test]
    async fn test_rust_struct_extraction() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.rs");
        fs::write(
            &test_file,
            "struct Point { x: i32, y: i32 }\npub struct Config {}\n",
        )
        .unwrap();

        let tool = CodeSymbolsTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.rs",
            "symbol_type": "struct"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: SymbolsOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.symbols.len(), 2);
        assert_eq!(output.symbols[0].name, "Point");
        assert_eq!(output.symbols[1].name, "Config");
    }

    #[tokio::test]
    async fn test_typescript_extraction() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.ts");
        fs::write(
            &test_file,
            "function greet(name: string) {}\nclass User {}\ninterface Config {}\n",
        )
        .unwrap();

        let tool = CodeSymbolsTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.ts"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: SymbolsOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.language, "typescript");
        assert_eq!(output.symbols.len(), 3);
    }

    #[tokio::test]
    async fn test_python_extraction() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.py");
        fs::write(
            &test_file,
            "def hello(name):\n    pass\n\nclass MyClass:\n    pass\n",
        )
        .unwrap();

        let tool = CodeSymbolsTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.py"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: SymbolsOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.language, "python");
        assert_eq!(output.symbols.len(), 2);
        assert_eq!(output.symbols[0].symbol_type, "function");
        assert_eq!(output.symbols[1].symbol_type, "class");
    }

    #[tokio::test]
    async fn test_symbol_type_filter() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.rs");
        fs::write(
            &test_file,
            "fn func1() {}\nstruct Struct1 {}\nfn func2() {}\n",
        )
        .unwrap();

        let tool = CodeSymbolsTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.rs",
            "symbol_type": "function"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: SymbolsOutput = serde_json::from_value(result.output).unwrap();
        // Should only extract functions, not the struct
        assert_eq!(output.symbols.len(), 2);
        assert!(output.symbols.iter().all(|s| s.symbol_type == "function"));
    }
}
