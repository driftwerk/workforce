use eyre::{eyre, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::graph::entities::Language;

/// Configuration for the file crawler.
#[derive(Debug, Clone)]
pub struct CrawlerConfig {
    /// Root directory to crawl.
    pub root: PathBuf,
    /// Maximum file size to process (default: 1MB).
    pub max_file_size: u64,
    /// Additional glob patterns to exclude (beyond .gitignore).
    pub exclude_patterns: Vec<String>,
    /// Language extensions to index. None = all supported.
    pub language_filter: Option<Vec<String>>,
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            max_file_size: 1_048_576, // 1 MB
            exclude_patterns: vec![
                "**/vendor/**".to_string(),
                "**/node_modules/**".to_string(),
                "**/target/**".to_string(),
                "**/.git/**".to_string(),
                "**/generated/**".to_string(),
                "**/*.min.js".to_string(),
                "**/*.min.css".to_string(),
            ],
            language_filter: None,
        }
    }
}

/// A discovered file ready for indexing.
#[derive(Debug, Clone)]
pub struct CrawledFile {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Path relative to the repository root.
    pub relative_path: String,
    /// Detected programming language.
    pub language: Language,
    /// File size in bytes.
    pub size_bytes: u64,
    /// BLAKE3 content hash for change detection.
    pub content_hash: String,
}

impl CrawledFile {
    pub fn new(path: PathBuf, root: &Path) -> Result<Self> {
        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let language = Language::from_extension(extension);

        let metadata = std::fs::metadata(&path)?;
        let size_bytes = metadata.len();

        // Compute BLAKE3 hash of file content
        let content = std::fs::read(&path)?;
        let content_hash = blake3::hash(&content).to_hex().to_string();

        Ok(Self {
            path,
            relative_path,
            language,
            size_bytes,
            content_hash,
        })
    }
}

/// File crawler that discovers indexable files in a repository.
pub struct FileCrawler {
    config: CrawlerConfig,
}

impl FileCrawler {
    pub fn new(config: CrawlerConfig) -> Self {
        Self { config }
    }

    /// Crawl the repository and return all indexable files.
    pub async fn crawl(&self) -> Result<Vec<CrawledFile>> {
        let root = self.config.root.clone();
        if !root.exists() {
            return Err(eyre!("Root path does not exist: {}", root.display()));
        }

        let config = self.config.clone();

        // Run the walk in a blocking task since it's IO-heavy
        let files =
            tokio::task::spawn_blocking(move || Self::walk_directory(&root, &config)).await??;

        info!(
            "Crawled {} indexable files from {}",
            files.len(),
            self.config.root.display()
        );

        Ok(files)
    }

    fn walk_directory(root: &Path, config: &CrawlerConfig) -> Result<Vec<CrawledFile>> {
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(true)       // Skip hidden files
            .git_ignore(true)   // Respect .gitignore
            .git_global(true)   // Respect global gitignore
            .git_exclude(true); // Respect .git/info/exclude

        let walker = builder.build();
        let mut files = Vec::new();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    debug!("Walk error: {}", e);
                    continue;
                }
            };

            // Skip directories
            if entry.file_type().is_none_or(|ft| ft.is_dir()) {
                continue;
            }

            let path = entry.path().to_path_buf();

            // Check exclude patterns
            let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
            if config.exclude_patterns.iter().any(|p| {
                glob::Pattern::new(p)
                    .map(|pat| pat.matches(&relative))
                    .unwrap_or(false)
            }) {
                continue;
            }

            // Check file size
            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > config.max_file_size {
                debug!(
                    "Skipping large file: {} ({} bytes)",
                    path.display(),
                    metadata.len()
                );
                continue;
            }

            // Check if file is binary (null byte in first 8KB)
            if Self::is_binary(&path) {
                continue;
            }

            // Check language filter
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if let Some(ref filter) = config.language_filter {
                let lang = Language::from_extension(extension);
                if !filter.contains(&lang.as_str().to_string()) {
                    continue;
                }
            }

            match CrawledFile::new(path, root) {
                Ok(file) => files.push(file),
                Err(e) => {
                    debug!("Failed to process file: {}", e);
                }
            }
        }

        // Sort by path for deterministic ordering
        files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        Ok(files)
    }

    /// Create a `CrawledFile` for a single relative path, applying the same
    /// filters (exclude patterns, size limits, binary detection) as the full crawl.
    /// Returns `Ok(None)` if the file is excluded, too large, binary, or missing.
    pub async fn crawl_file(&self, relative_path: &str) -> Result<Option<CrawledFile>> {
        let root = self.config.root.clone();
        let config = self.config.clone();
        let relative_path = relative_path.to_string();

        tokio::task::spawn_blocking(move || {
            let abs_path = root.join(&relative_path);

            // Check file exists
            if !abs_path.exists() || !abs_path.is_file() {
                return Ok(None);
            }

            // Check exclude patterns
            if config.exclude_patterns.iter().any(|p| {
                glob::Pattern::new(p)
                    .map(|pat| pat.matches(&relative_path))
                    .unwrap_or(false)
            }) {
                return Ok(None);
            }

            // Check file size
            let metadata = std::fs::metadata(&abs_path)?;
            if metadata.len() > config.max_file_size {
                return Ok(None);
            }

            // Check if binary
            if Self::is_binary(&abs_path) {
                return Ok(None);
            }

            // Check language filter
            let extension = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if let Some(ref filter) = config.language_filter {
                let lang = Language::from_extension(extension);
                if !filter.contains(&lang.as_str().to_string()) {
                    return Ok(None);
                }
            }

            CrawledFile::new(abs_path, &root).map(Some)
        })
        .await?
    }

    /// Check if a file is binary by looking for null bytes in the first 8KB.
    fn is_binary(path: &Path) -> bool {
        use std::io::Read;
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return true,
        };
        let mut buffer = [0u8; 8192];
        let bytes_read = match file.read(&mut buffer) {
            Ok(n) => n,
            Err(_) => return true,
        };
        buffer[..bytes_read].contains(&0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn crawl_basic_directory() {
        let dir = TempDir::new().unwrap();

        // Create some test files
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("lib.rs"), "pub mod foo;").unwrap();
        fs::write(dir.path().join("test.py"), "def test(): pass").unwrap();
        fs::write(dir.path().join("readme.md"), "# Hello").unwrap();

        let config = CrawlerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let crawler = FileCrawler::new(config);
        let files = crawler.crawl().await.unwrap();

        assert_eq!(files.len(), 4);

        let rust_files: Vec<_> = files
            .iter()
            .filter(|f| f.language == Language::Rust)
            .collect();
        assert_eq!(rust_files.len(), 2);
    }

    #[tokio::test]
    async fn crawl_skips_large_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("small.rs"), "fn main() {}").unwrap();
        // Create a file larger than the limit
        let large_content = "x".repeat(2_000_000);
        fs::write(dir.path().join("large.rs"), large_content).unwrap();

        let config = CrawlerConfig {
            root: dir.path().to_path_buf(),
            max_file_size: 1_048_576,
            ..Default::default()
        };
        let crawler = FileCrawler::new(config);
        let files = crawler.crawl().await.unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, "small.rs");
    }

    #[test]
    fn content_hash_deterministic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn test() {}").unwrap();

        let f1 = CrawledFile::new(path.clone(), dir.path()).unwrap();
        let f2 = CrawledFile::new(path, dir.path()).unwrap();
        assert_eq!(f1.content_hash, f2.content_hash);
    }
}
