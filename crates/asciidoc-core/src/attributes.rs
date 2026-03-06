use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::types::AttributeDef;

#[derive(Debug, Clone)]
pub struct AttributeSource {
    pub file: PathBuf,
    pub line: usize,
}

#[derive(Debug, Clone)]
struct AttributeEntry {
    value: String,
    source: Option<AttributeSource>,
}

pub struct AttributeRegistry {
    builtin: HashMap<String, String>,
    config_chains: HashMap<PathBuf, HashMap<String, AttributeEntry>>,
    per_file: HashMap<PathBuf, HashMap<String, AttributeEntry>>,
    workspace_root: PathBuf,
}

impl AttributeRegistry {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let mut builtin = HashMap::new();
        builtin.insert("docdir".into(), root.display().to_string());
        Self {
            builtin,
            config_chains: HashMap::new(),
            per_file: HashMap::new(),
            workspace_root: root.to_path_buf(),
        }
    }

    /// Ensure a config chain is computed and cached for the given directory.
    ///
    /// Walks from `workspace_root` down to `dir`, merging `.asciidoctorconfig` files
    /// along the way. Idempotent — returns early if already cached.
    pub fn ensure_config_for_dir(&mut self, dir: &Path) {
        let Ok(dir) = dir.canonicalize() else {
            return;
        };
        let Ok(ws_root) = self.workspace_root.canonicalize() else {
            return;
        };

        // Guard: dir must be under workspace_root
        if !dir.starts_with(&ws_root) {
            return;
        }

        // Idempotent: already cached
        if self.config_chains.contains_key(&dir) {
            return;
        }

        // Collect directories from workspace_root to dir (inclusive), walking upward then reversing
        let mut chain_dirs = Vec::new();
        let mut current = dir.clone();
        loop {
            chain_dirs.push(current.clone());
            if current == ws_root {
                break;
            }
            match current.parent() {
                Some(parent) => current = parent.to_path_buf(),
                None => break,
            }
        }
        chain_dirs.reverse(); // now root → dir order

        // Merge configs from root to dir (last wins)
        // First: load ~/.asciidoctorconfig from home dir (lowest priority in chain)
        let mut merged: HashMap<String, AttributeEntry> = HashMap::new();
        if let Some(home_dir) = std::env::var("HOME").ok().map(PathBuf::from) {
            let home_config = home_dir.join(".asciidoctorconfig");
            if let Ok(content) = std::fs::read_to_string(&home_config) {
                let canonical_home = home_dir.canonicalize().unwrap_or(home_dir);
                merged.insert(
                    "asciidoctorconfigdir".into(),
                    AttributeEntry {
                        value: canonical_home.display().to_string(),
                        source: None,
                    },
                );
                for (key, value, line) in parse_asciidoctorconfig(&content) {
                    let substituted = substitute_with(&merged, &value);
                    let resolved = canonicalize_path_value(&substituted, &canonical_home);
                    merged.insert(
                        key,
                        AttributeEntry {
                            value: resolved,
                            source: Some(AttributeSource {
                                file: home_config.clone(),
                                line,
                            }),
                        },
                    );
                }
            }
        }

        for chain_dir in &chain_dirs {
            let config_path = chain_dir.join(".asciidoctorconfig");
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                // Synthetic attribute: {asciidoctorconfigdir} = directory of this config
                merged.insert(
                    "asciidoctorconfigdir".into(),
                    AttributeEntry {
                        value: chain_dir.display().to_string(),
                        source: None,
                    },
                );

                for (key, value, line) in parse_asciidoctorconfig(&content) {
                    let substituted = substitute_with(&merged, &value);
                    let resolved = canonicalize_path_value(&substituted, chain_dir);
                    merged.insert(
                        key,
                        AttributeEntry {
                            value: resolved,
                            source: Some(AttributeSource {
                                file: config_path.clone(),
                                line,
                            }),
                        },
                    );
                }
            }
        }

        self.config_chains.insert(dir, merged);
    }

    pub fn register_file_attributes(&mut self, file: &Path, attrs: &[AttributeDef]) {
        let map: HashMap<String, AttributeEntry> = attrs
            .iter()
            .map(|a| {
                (
                    a.name.clone(),
                    AttributeEntry {
                        value: a.value.clone(),
                        source: Some(AttributeSource {
                            file: file.to_path_buf(),
                            line: a.line,
                        }),
                    },
                )
            })
            .collect();
        self.per_file.insert(file.to_path_buf(), map);
    }

    /// Return all attributes merged across layers with priority: `per_file` > config > builtin.
    /// Later layers override earlier ones for the same key.
    #[must_use]
    pub fn all_attributes(&self, file: &Path) -> Vec<(&str, &str)> {
        let mut merged: HashMap<&str, &str> = self
            .builtin
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        if let Some(dir) = file.parent() {
            if let Some(config) = self.config_chains.get(dir) {
                for (k, entry) in config {
                    merged.insert(k.as_str(), entry.value.as_str());
                }
            }
        }

        if let Some(file_attrs) = self.per_file.get(file) {
            for (k, entry) in file_attrs {
                merged.insert(k.as_str(), entry.value.as_str());
            }
        }

        merged.into_iter().collect()
    }

    /// Resolve an attribute by name with priority: Document-Header > Config > Built-in
    #[must_use]
    pub fn resolve(&self, file: &Path, name: &str) -> Option<&str> {
        self.per_file
            .get(file)
            .and_then(|m| m.get(name))
            .map(|e| e.value.as_str())
            .or_else(|| {
                let dir = file.parent()?;
                self.config_chains
                    .get(dir)?
                    .get(name)
                    .map(|e| e.value.as_str())
            })
            .or_else(|| self.builtin.get(name).map(std::string::String::as_str))
    }

    /// Resolve the source location of an attribute definition.
    /// Same priority as `resolve()`: `per_file` > config > builtin.
    /// Builtins have no source location and return None.
    #[must_use]
    pub fn resolve_source(&self, file: &Path, name: &str) -> Option<AttributeSource> {
        self.per_file
            .get(file)
            .and_then(|m| m.get(name))
            .and_then(|e| e.source.clone())
            .or_else(|| {
                let dir = file.parent()?;
                self.config_chains
                    .get(dir)?
                    .get(name)
                    .and_then(|e| e.source.clone())
            })
        // builtins: no source → implicitly None
    }

    /// Substitute all `{attr}` references in a string using `resolve()`.
    #[must_use]
    pub fn substitute(&self, file: &Path, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '{' {
                let mut name = String::new();
                let mut found_close = false;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        found_close = true;
                        break;
                    }
                    name.push(inner);
                }
                if found_close && !name.is_empty() {
                    if let Some(val) = self.resolve(file, &name) {
                        result.push_str(val);
                    } else {
                        // Undefined attribute: leave as-is
                        result.push('{');
                        result.push_str(&name);
                        result.push('}');
                    }
                } else {
                    result.push('{');
                    result.push_str(&name);
                }
            } else {
                result.push(ch);
            }
        }

        result
    }
}

/// Substitute all `{attr}` references in a string using the provided attribute map.
/// Same logic as `AttributeRegistry::substitute()`, but without `self` — works only
/// on the given `HashMap`. Used by `ensure_config_for_dir()` for config-chain merging.
#[must_use]
fn substitute_with(attrs: &HashMap<String, AttributeEntry>, input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut name = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    found_close = true;
                    break;
                }
                name.push(inner);
            }
            if found_close && !name.is_empty() {
                if let Some(entry) = attrs.get(&name) {
                    result.push_str(&entry.value);
                } else {
                    result.push('{');
                    result.push_str(&name);
                    result.push('}');
                }
            } else {
                result.push('{');
                result.push_str(&name);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Parse :key: value lines from an .asciidoctorconfig file.
/// Returns (key, value, 1-indexed line number) tuples.
fn parse_asciidoctorconfig(content: &str) -> Vec<(String, String, usize)> {
    let mut result = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.starts_with(':') && line.len() > 1 {
            if let Some(rest) = line.strip_prefix(':') {
                if let Some((key, value)) = rest.split_once(':') {
                    let key = key.trim();
                    let value = value.trim();
                    if !key.is_empty() {
                        result.push((key.to_string(), value.to_string(), idx + 1));
                    }
                }
            }
        }
    }
    result
}

/// Heuristic: if a value looks like a relative path (starts with `.` or `../`),
/// canonicalize it relative to the config file's directory.
fn canonicalize_path_value(value: &str, config_dir: &Path) -> String {
    if value.starts_with("../") || value.starts_with("./") {
        let resolved = config_dir.join(value);
        match resolved.canonicalize() {
            Ok(canonical) => canonical.display().to_string(),
            // Path doesn't exist yet — normalize without canonicalize
            Err(_) => resolved.display().to_string(),
        }
    } else if Path::new(value).is_absolute() && value.contains("..") {
        match Path::new(value).canonicalize() {
            Ok(canonical) => canonical.display().to_string(),
            Err(_) => value.to_string(),
        }
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_builtin_docdir() {
        let registry = AttributeRegistry::new(Path::new("/tmp/docs"));
        let dummy = Path::new("/tmp/docs/file.adoc");
        assert_eq!(registry.resolve(dummy, "docdir"), Some("/tmp/docs"));
    }

    #[test]
    fn test_config_loader_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);

        let dummy = root.join("any.adoc");
        assert_eq!(registry.resolve(&dummy, "imagesdir"), Some("images"));
    }

    #[test]
    fn test_config_path_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs = root.join("project").join("docs");
        let src = root.join("project").join("src");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&src).unwrap();

        fs::write(docs.join(".asciidoctorconfig"), ":source-dir: ../src\n").unwrap();

        let mut registry = AttributeRegistry::new(&docs);
        registry.ensure_config_for_dir(&docs);

        let dummy = docs.join("any.adoc");
        let resolved = registry.resolve(&dummy, "source-dir").unwrap();
        assert_eq!(resolved, src.display().to_string());
    }

    #[test]
    fn test_config_hierarchy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let parent = root.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        fs::write(
            parent.join(".asciidoctorconfig"),
            ":imagesdir: images\n:chapdir: chapters\n",
        )
        .unwrap();
        fs::write(child.join(".asciidoctorconfig"), ":imagesdir: img\n").unwrap();

        // workspace_root = parent so the walk covers parent → child
        let mut registry = AttributeRegistry::new(&parent);
        registry.ensure_config_for_dir(&child);

        let dummy = child.join("any.adoc");
        assert_eq!(registry.resolve(&dummy, "imagesdir"), Some("img"));
        assert_eq!(registry.resolve(&dummy, "chapdir"), Some("chapters"));
    }

    #[test]
    fn test_register_file_attributes() {
        let mut registry = AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("/tmp/doc.adoc");
        let attrs = vec![
            AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 1,
            },
            AttributeDef {
                name: "version".into(),
                value: "1.0".into(),
                line: 2,
            },
        ];
        registry.register_file_attributes(file, &attrs);
        assert_eq!(registry.resolve(file, "author"), Some("Alice"));
        assert_eq!(registry.resolve(file, "version"), Some("1.0"));
    }

    #[test]
    fn test_resolve_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let mut registry = AttributeRegistry::new(&root);
        // Simulate config layer via config_chains
        let mut config_map = HashMap::new();
        config_map.insert(
            "docdir".into(),
            AttributeEntry {
                value: "/config".into(),
                source: None,
            },
        );
        registry.config_chains.insert(root.clone(), config_map);

        let file = root.join("doc.adoc");
        // Config overrides built-in
        assert_eq!(registry.resolve(&file, "docdir"), Some("/config"));

        // Doc-header overrides config
        let attrs = vec![AttributeDef {
            name: "docdir".into(),
            value: "/header".into(),
            line: 1,
        }];
        registry.register_file_attributes(&file, &attrs);
        assert_eq!(registry.resolve(&file, "docdir"), Some("/header"));
    }

    #[test]
    fn test_resolve_fallback() {
        let registry = AttributeRegistry::new(Path::new("/root"));
        let file = Path::new("/root/any.adoc");
        assert_eq!(registry.resolve(file, "docdir"), Some("/root"));
        assert_eq!(registry.resolve(file, "nonexistent"), None);
    }

    #[test]
    fn test_substitute_in_path() {
        let registry = AttributeRegistry::new(Path::new("/project/docs"));
        let file = Path::new("/project/docs/any.adoc");
        let result = registry.substitute(file, "{docdir}/file.adoc");
        assert_eq!(result, "/project/docs/file.adoc");
    }

    #[test]
    fn test_substitute_undefined_attr() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("/tmp/any.adoc");
        let result = registry.substitute(file, "{unknown}/path");
        assert_eq!(result, "{unknown}/path");
    }

    // --- resolve_source() tests ---

    #[test]
    fn test_resolve_source_doc_header() {
        let mut registry = AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("/tmp/doc.adoc");
        let attrs = vec![AttributeDef {
            name: "author".into(),
            value: "Alice".into(),
            line: 1,
        }];
        registry.register_file_attributes(file, &attrs);

        let source = registry.resolve_source(file, "author").unwrap();
        assert_eq!(source.file, file);
        assert_eq!(source.line, 1);
    }

    #[test]
    fn test_resolve_source_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);

        let dummy = root.join("any.adoc");
        let source = registry.resolve_source(&dummy, "imagesdir").unwrap();
        assert_eq!(source.file, root.join(".asciidoctorconfig"));
        assert_eq!(source.line, 1);
    }

    #[test]
    fn test_resolve_source_builtin() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let dummy = Path::new("/tmp/any.adoc");
        assert!(registry.resolve_source(dummy, "docdir").is_none());
    }

    // --- all_attributes() tests ---

    #[test]
    fn test_all_attributes_merges_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let mut registry = AttributeRegistry::new(&root);
        // Config layer via config_chains
        let mut config_map = HashMap::new();
        config_map.insert(
            "imagesdir".into(),
            AttributeEntry {
                value: "images".into(),
                source: None,
            },
        );
        registry.config_chains.insert(root.clone(), config_map);

        // Per-file layer
        let file = root.join("doc.adoc");
        let attrs = vec![
            AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 1,
            },
            AttributeDef {
                name: "imagesdir".into(),
                value: "img".into(),
                line: 2,
            },
        ];
        registry.register_file_attributes(&file, &attrs);

        let all = registry.all_attributes(&file);
        let map: HashMap<&str, &str> = all.into_iter().collect();

        let root_str = root.display().to_string();
        assert_eq!(map.get("docdir"), Some(&root_str.as_str()));
        assert_eq!(map.get("imagesdir"), Some(&"img"));
        assert_eq!(map.get("author"), Some(&"Alice"));
    }

    // --- {asciidoctorconfigdir} tests ---

    #[test]
    fn test_asciidoctorconfigdir_injected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs = root.join("project").join("docs");
        let src = root.join("project").join("src");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&src).unwrap();

        fs::write(
            docs.join(".asciidoctorconfig"),
            ":source-dir: {asciidoctorconfigdir}/../src\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(&docs);
        registry.ensure_config_for_dir(&docs);

        let dummy = docs.join("any.adoc");

        assert_eq!(
            registry.resolve(&dummy, "asciidoctorconfigdir"),
            Some(docs.display().to_string()).as_deref()
        );
        assert_eq!(
            registry.resolve(&dummy, "source-dir"),
            Some(src.display().to_string()).as_deref()
        );
    }

    #[test]
    fn test_asciidoctorconfigdir_hierarchy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let parent = root.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        fs::write(
            parent.join(".asciidoctorconfig"),
            ":parent-dir: {asciidoctorconfigdir}/data\n",
        )
        .unwrap();
        fs::write(
            child.join(".asciidoctorconfig"),
            ":child-dir: {asciidoctorconfigdir}/output\n",
        )
        .unwrap();

        // workspace_root = parent so walk covers parent → child
        let mut registry = AttributeRegistry::new(&parent);
        registry.ensure_config_for_dir(&child);

        let dummy = child.join("any.adoc");

        assert_eq!(
            registry.resolve(&dummy, "parent-dir"),
            Some(format!("{}/data", parent.display())).as_deref()
        );
        assert_eq!(
            registry.resolve(&dummy, "child-dir"),
            Some(format!("{}/output", child.display())).as_deref()
        );
        assert_eq!(
            registry.resolve(&dummy, "asciidoctorconfigdir"),
            Some(child.display().to_string()).as_deref()
        );
    }

    // --- substitute_with() tests ---

    #[test]
    fn test_substitute_with_basic() {
        let mut attrs: HashMap<String, AttributeEntry> = HashMap::new();
        attrs.insert(
            "imagesdir".into(),
            AttributeEntry {
                value: "images".into(),
                source: None,
            },
        );
        let result = substitute_with(&attrs, "path/{imagesdir}/file.png");
        assert_eq!(result, "path/images/file.png");
    }

    #[test]
    fn test_substitute_with_chained() {
        let mut attrs: HashMap<String, AttributeEntry> = HashMap::new();
        attrs.insert(
            "base".into(),
            AttributeEntry {
                value: "/root".into(),
                source: None,
            },
        );
        attrs.insert(
            "sub".into(),
            AttributeEntry {
                value: "docs".into(),
                source: None,
            },
        );
        let result = substitute_with(&attrs, "{base}/{sub}/index.adoc");
        assert_eq!(result, "/root/docs/index.adoc");
    }

    #[test]
    fn test_substitute_with_unknown_attr() {
        let attrs: HashMap<String, AttributeEntry> = HashMap::new();
        let result = substitute_with(&attrs, "{unknown}/path");
        assert_eq!(result, "{unknown}/path");
    }

    #[test]
    fn test_all_attributes_empty_registry() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("/tmp/doc.adoc");
        let all = registry.all_attributes(file);
        let map: HashMap<&str, &str> = all.into_iter().collect();

        assert_eq!(map.len(), 1);
        assert_eq!(map.get("docdir"), Some(&"/tmp"));
    }

    // --- Review Corrections: Edge Case Tests ---

    #[test]
    fn test_ensure_config_for_dir_file_in_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":theme: dark\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);

        let file = root.join("index.adoc");
        assert_eq!(registry.resolve(&file, "theme"), Some("dark"));
    }

    #[test]
    fn test_ensure_config_for_dir_empty_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        // No .asciidoctorconfig anywhere

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&sub);

        let file = sub.join("doc.adoc");
        // Only builtin should be available
        assert_eq!(
            registry.resolve(&file, "docdir"),
            Some(root.display().to_string()).as_deref()
        );
        assert_eq!(registry.resolve(&file, "imagesdir"), None);
    }

    #[test]
    fn test_ensure_config_for_dir_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":key: value\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        registry.ensure_config_for_dir(&root); // second call — idempotent

        let file = root.join("doc.adoc");
        assert_eq!(registry.resolve(&file, "key"), Some("value"));
    }

    #[test]
    fn test_ensure_config_for_dir_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let inside = root.join("inside");
        fs::create_dir_all(&inside).unwrap();

        let mut registry = AttributeRegistry::new(&inside);
        // root is outside workspace_root (which is inside/)
        registry.ensure_config_for_dir(&root);

        // Should not have cached anything for root
        assert!(!registry.config_chains.contains_key(&root));
    }

    // --- Home-Dir ~/.asciidoctorconfig tests (adoc-0074) ---

    // Mutex to serialize tests that modify the HOME env var
    static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_home_dir_config_loaded() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("fakehome");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        fs::write(
            fake_home.join(".asciidoctorconfig"),
            ":home-attr: home-val\n",
        )
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &fake_home);

        let mut registry = AttributeRegistry::new(&workspace);
        registry.ensure_config_for_dir(&workspace);

        let file = workspace.join("doc.adoc");
        assert_eq!(registry.resolve(&file, "home-attr"), Some("home-val"));

        // Restore HOME
        match original_home {
            Some(val) => std::env::set_var("HOME", val),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn test_home_dir_config_overridden_by_project() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("fakehome");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        fs::write(fake_home.join(".asciidoctorconfig"), ":shared: home-val\n").unwrap();
        fs::write(
            workspace.join(".asciidoctorconfig"),
            ":shared: project-val\n",
        )
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &fake_home);

        let mut registry = AttributeRegistry::new(&workspace);
        registry.ensure_config_for_dir(&workspace);

        let file = workspace.join("doc.adoc");
        assert_eq!(registry.resolve(&file, "shared"), Some("project-val"));

        match original_home {
            Some(val) => std::env::set_var("HOME", val),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn test_home_dir_config_missing_is_ok() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("fakehome_empty");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        // No .asciidoctorconfig in fake_home

        fs::write(workspace.join(".asciidoctorconfig"), ":project-key: pval\n").unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &fake_home);

        let mut registry = AttributeRegistry::new(&workspace);
        registry.ensure_config_for_dir(&workspace);

        let file = workspace.join("doc.adoc");
        // Project config still works
        assert_eq!(registry.resolve(&file, "project-key"), Some("pval"));
        // No error, no crash

        match original_home {
            Some(val) => std::env::set_var("HOME", val),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn test_home_dir_asciidoctorconfigdir() {
        let _lock = HOME_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("fakehome");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let fake_home = fake_home.canonicalize().unwrap();

        fs::write(
            fake_home.join(".asciidoctorconfig"),
            ":home-snippets: {asciidoctorconfigdir}/snippets\n",
        )
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &fake_home);

        let mut registry = AttributeRegistry::new(&workspace);
        registry.ensure_config_for_dir(&workspace);

        let file = workspace.join("doc.adoc");
        assert_eq!(
            registry.resolve(&file, "home-snippets"),
            Some(format!("{}/snippets", fake_home.display())).as_deref()
        );

        match original_home {
            Some(val) => std::env::set_var("HOME", val),
            None => std::env::remove_var("HOME"),
        }
    }

    // --- adoc-0072: Config-Chain with LSP root_uri ---

    #[test]
    fn test_config_chain_with_lsp_root() {
        // Simulates LSP root_uri pointing to a specific directory.
        // .asciidoctorconfig in that directory should be loaded for files in subdirs.
        let tmp = tempfile::tempdir().unwrap();
        let lsp_root = tmp.path().join("project");
        let sub = lsp_root.join("chapters");
        fs::create_dir_all(&sub).unwrap();

        fs::write(
            lsp_root.join(".asciidoctorconfig"),
            ":imagesdir: images\n:source-dir: src\n",
        )
        .unwrap();

        let lsp_root = lsp_root.canonicalize().unwrap();
        let sub = sub.canonicalize().unwrap();

        let mut registry = AttributeRegistry::new(&lsp_root);
        registry.ensure_config_for_dir(&sub);

        let file_in_sub = sub.join("chapter1.adoc");
        assert_eq!(
            registry.resolve(&file_in_sub, "imagesdir"),
            Some("images"),
            "Config from LSP root should be loaded for files in subdirectories"
        );
        assert_eq!(
            registry.resolve(&file_in_sub, "source-dir"),
            Some("src"),
            "All attributes from .asciidoctorconfig at LSP root should resolve"
        );
    }
}
