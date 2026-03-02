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
    config: HashMap<String, AttributeEntry>,
    per_file: HashMap<PathBuf, HashMap<String, AttributeEntry>>,
}

impl AttributeRegistry {
    pub fn new(root: &Path) -> Self {
        let mut builtin = HashMap::new();
        builtin.insert("docdir".into(), root.display().to_string());
        Self {
            builtin,
            config: HashMap::new(),
            per_file: HashMap::new(),
        }
    }

    /// Load attributes from .asciidoctorconfig files, searching upward from root.
    /// Configs closer to root override values from parent directories.
    /// Path values (containing `/` or starting with `.`) are canonicalized
    /// relative to the config file's directory.
    pub fn load_config(&mut self, root: &Path) {
        let start_dir = match root.canonicalize() {
            Ok(d) => d,
            Err(_) => return,
        };

        let mut config_contents: Vec<(PathBuf, String)> = Vec::new();
        let mut current = Some(start_dir.as_path());
        while let Some(dir) = current {
            let config_path = dir.join(".asciidoctorconfig");
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                config_contents.push((dir.to_path_buf(), content));
            }
            current = dir.parent();
        }

        // Merge from root to leaf (later entries = closer to file = override)
        for (config_dir, content) in config_contents.iter().rev() {
            let config_file = config_dir.join(".asciidoctorconfig");
            for (key, value, line) in parse_asciidoctorconfig(content) {
                let resolved = canonicalize_path_value(&value, config_dir);
                self.config.insert(
                    key,
                    AttributeEntry {
                        value: resolved,
                        source: Some(AttributeSource {
                            file: config_file.clone(),
                            line,
                        }),
                    },
                );
            }
        }
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

    /// Return all attributes merged across layers with priority: per_file > config > builtin.
    /// Later layers override earlier ones for the same key.
    pub fn all_attributes(&self, file: &Path) -> Vec<(String, String)> {
        let mut merged: HashMap<String, String> = self
            .builtin
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for (k, entry) in &self.config {
            merged.insert(k.clone(), entry.value.clone());
        }

        if let Some(file_attrs) = self.per_file.get(file) {
            for (k, entry) in file_attrs {
                merged.insert(k.clone(), entry.value.clone());
            }
        }

        merged.into_iter().collect()
    }

    /// Resolve an attribute by name with priority: Document-Header > Config > Built-in
    pub fn resolve(&self, file: &Path, name: &str) -> Option<&str> {
        self.per_file
            .get(file)
            .and_then(|m| m.get(name))
            .map(|e| e.value.as_str())
            .or_else(|| self.config.get(name).map(|e| e.value.as_str()))
            .or_else(|| self.builtin.get(name).map(|s| s.as_str()))
    }

    /// Resolve the source location of an attribute definition.
    /// Same priority as resolve(): per_file > config > builtin.
    /// Builtins have no source location and return None.
    pub fn resolve_source(&self, file: &Path, name: &str) -> Option<AttributeSource> {
        self.per_file
            .get(file)
            .and_then(|m| m.get(name))
            .and_then(|e| e.source.clone())
            .or_else(|| self.config.get(name).and_then(|e| e.source.clone()))
        // builtins: no source → implicitly None
    }

    /// Substitute all `{attr}` references in a string using resolve().
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
        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: images\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        registry.load_config(tmp.path());

        let dummy = Path::new("any.adoc");
        assert_eq!(registry.resolve(dummy, "imagesdir"), Some("images"));
    }

    #[test]
    fn test_config_path_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("project").join("docs");
        let src = tmp.path().join("project").join("src");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&src).unwrap();

        fs::write(docs.join(".asciidoctorconfig"), ":source-dir: ../src\n").unwrap();

        let mut registry = AttributeRegistry::new(&docs);
        registry.load_config(&docs);

        let dummy = Path::new("any.adoc");
        let resolved = registry.resolve(dummy, "source-dir").unwrap();
        let canonical_src = src.canonicalize().unwrap();
        assert_eq!(resolved, canonical_src.display().to_string());
    }

    #[test]
    fn test_config_hierarchy() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        fs::write(
            parent.join(".asciidoctorconfig"),
            ":imagesdir: images\n:chapdir: chapters\n",
        )
        .unwrap();
        fs::write(child.join(".asciidoctorconfig"), ":imagesdir: img\n").unwrap();

        let mut registry = AttributeRegistry::new(&child);
        registry.load_config(&child);

        let dummy = Path::new("any.adoc");
        assert_eq!(registry.resolve(dummy, "imagesdir"), Some("img"));
        assert_eq!(registry.resolve(dummy, "chapdir"), Some("chapters"));
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
        let mut registry = AttributeRegistry::new(Path::new("/root"));
        // Built-in: docdir = /root
        registry.config.insert(
            "docdir".into(),
            AttributeEntry {
                value: "/config".into(),
                source: None,
            },
        );

        let file = Path::new("/root/doc.adoc");
        // Config overrides built-in
        assert_eq!(registry.resolve(file, "docdir"), Some("/config"));

        // Doc-header overrides config
        let attrs = vec![AttributeDef {
            name: "docdir".into(),
            value: "/header".into(),
            line: 1,
        }];
        registry.register_file_attributes(file, &attrs);
        assert_eq!(registry.resolve(file, "docdir"), Some("/header"));
    }

    #[test]
    fn test_resolve_fallback() {
        let registry = AttributeRegistry::new(Path::new("/root"));
        let file = Path::new("any.adoc");
        assert_eq!(registry.resolve(file, "docdir"), Some("/root"));
        assert_eq!(registry.resolve(file, "nonexistent"), None);
    }

    #[test]
    fn test_substitute_in_path() {
        let registry = AttributeRegistry::new(Path::new("/project/docs"));
        let file = Path::new("any.adoc");
        let result = registry.substitute(file, "{docdir}/file.adoc");
        assert_eq!(result, "/project/docs/file.adoc");
    }

    #[test]
    fn test_substitute_undefined_attr() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("any.adoc");
        let result = registry.substitute(file, "{unknown}/path");
        assert_eq!(result, "{unknown}/path");
    }

    // --- Task 1.1: resolve_source() tests ---

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
        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: images\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        registry.load_config(tmp.path());

        let dummy = Path::new("any.adoc");
        let source = registry.resolve_source(dummy, "imagesdir").unwrap();
        let config_path = tmp
            .path()
            .join(".asciidoctorconfig")
            .canonicalize()
            .unwrap();
        assert_eq!(source.file, config_path);
        assert_eq!(source.line, 1);
    }

    #[test]
    fn test_resolve_source_builtin() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let dummy = Path::new("any.adoc");
        // Builtins have no source location
        assert!(registry.resolve_source(dummy, "docdir").is_none());
    }

    // --- Task 1.1: all_attributes() tests ---

    #[test]
    fn test_all_attributes_merges_layers() {
        let mut registry = AttributeRegistry::new(Path::new("/root"));
        // Config layer
        registry.config.insert(
            "imagesdir".into(),
            AttributeEntry {
                value: "images".into(),
                source: None,
            },
        );
        // Per-file layer
        let file = Path::new("/root/doc.adoc");
        let attrs = vec![
            AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 1,
            },
            // Override config key
            AttributeDef {
                name: "imagesdir".into(),
                value: "img".into(),
                line: 2,
            },
        ];
        registry.register_file_attributes(file, &attrs);

        let all = registry.all_attributes(file);
        let map: HashMap<&str, &str> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        // Builtin present
        assert_eq!(map.get("docdir"), Some(&"/root"));
        // Per-file overrides config
        assert_eq!(map.get("imagesdir"), Some(&"img"));
        // Per-file attribute present
        assert_eq!(map.get("author"), Some(&"Alice"));
    }

    #[test]
    fn test_all_attributes_empty_registry() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("/tmp/doc.adoc");
        let all = registry.all_attributes(file);
        let map: HashMap<&str, &str> = all.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        // Only builtin docdir
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("docdir"), Some(&"/tmp"));
    }
}
