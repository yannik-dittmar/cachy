//! Exclusions: gitignore patterns that keep matching files and directories on
//! the cache branch.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Compiled exclusion patterns. Unlike in a `.gitignore`, every pattern is
/// anchored at the cache branch root: `appdata/` matches only the top-level
/// `appdata` directory, and `**/` matches at any depth. A `#` is literal, not
/// a comment.
#[derive(Debug)]
pub struct Exclusions(Gitignore);

impl Exclusions {
    pub fn new(patterns: &[String]) -> Result<Exclusions, String> {
        let mut builder = GitignoreBuilder::new("");
        for pattern in patterns {
            if pattern.trim().is_empty() {
                return Err("exclude: a pattern is empty".to_owned());
            }
            builder
                .add_line(None, &anchored(pattern))
                .map_err(|e| format!("exclude: invalid pattern `{pattern}`: {e}"))?;
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

/// `pattern` with a leading `/`, after any `!`, which gitignore takes as an
/// anchor at the root.
fn anchored(pattern: &str) -> String {
    let (negation, rest) = match pattern.strip_prefix('!') {
        Some(rest) => ("!", rest),
        None => ("", pattern),
    };
    if rest.starts_with('/') {
        pattern.to_owned()
    } else {
        format!("{negation}/{rest}")
    }
}
