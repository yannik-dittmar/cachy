//! Exclusions: gitignore patterns that keep matching files and directories on
//! the cache branch.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Compiled exclusion patterns, each one a line of a `.gitignore` at the cache
/// branch root.
#[derive(Debug)]
pub struct Exclusions(Gitignore);

impl Exclusions {
    /// Compiles `patterns`, or says why one of them is invalid.
    pub fn new(patterns: &[String]) -> Result<Exclusions, String> {
        let mut builder = GitignoreBuilder::new("");
        for pattern in patterns {
            if pattern.trim().is_empty() {
                return Err("a pattern is empty".to_owned());
            }
            builder
                .add_line(None, pattern)
                .map_err(|e| format!("invalid pattern `{pattern}`: {e}"))?;
        }
        let gitignore = builder.build().map_err(|e| e.to_string())?;
        Ok(Exclusions(gitignore))
    }

    /// Whether the patterns exclude `rel`, a path relative to the cache
    /// branch root. Excluded directories are pruned from the walk, so their
    /// contents are never matched.
    pub fn excludes(&self, rel: &Path, is_dir: bool) -> bool {
        self.0.matched(rel, is_dir).is_ignore()
    }
}

impl Default for Exclusions {
    /// No patterns: nothing is excluded.
    fn default() -> Exclusions {
        Exclusions(Gitignore::empty())
    }
}
