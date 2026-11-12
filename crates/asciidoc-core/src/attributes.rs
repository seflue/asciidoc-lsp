use std::collections::{HashMap, HashSet};
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

/// One of the three lookup layers of [`AttributeRegistry`], in descending priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeLayer {
    /// Document-header attributes registered for exactly this file path.
    PerFile,
    /// Merged `.asciidoctorconfig` chain cached for the file's document directory.
    Config,
    /// Built-in attributes (`docdir`, `env-asciidoc-lsp`).
    Builtin,
}

/// A value found in one layer, with its definition site when the layer has one.
#[derive(Debug, Clone)]
pub struct AttributeHit {
    pub value: String,
    pub source: Option<AttributeSource>,
}

/// Per-file layer outcome of [`AttributeRegistry::explain`].
#[derive(Debug, Clone)]
pub struct PerFileLookup {
    /// Whether any attributes are registered under exactly the queried path.
    pub registered: bool,
    pub hit: Option<AttributeHit>,
}

/// Config layer outcome of [`AttributeRegistry::explain`].
#[derive(Debug, Clone)]
pub struct ConfigLookup {
    /// The file's document directory (see [`AttributeRegistry::doc_dir`]),
    /// unchanged: the directory whose config chain applies (for a config
    /// file there, without the file itself).
    pub dir: Option<PathBuf>,
    /// Whether a config chain is cached for the file.
    pub chain_cached: bool,
    pub hit: Option<AttributeHit>,
}

/// Layer-by-layer account of how one attribute name resolves for one file path.
///
/// Uses the same lookup keys and priority as [`AttributeRegistry::resolve`].
#[derive(Debug, Clone)]
pub struct AttributeExplanation {
    pub per_file: PerFileLookup,
    pub config: ConfigLookup,
    pub builtin: Option<String>,
    /// The layer `resolve()` takes the value from; `None` when no layer has it.
    pub winner: Option<AttributeLayer>,
}

/// An attribute visible for a file, with the layer that supplies it.
#[derive(Debug, Clone)]
pub struct KnownAttribute {
    pub name: String,
    pub value: String,
    pub layer: AttributeLayer,
    pub source: Option<AttributeSource>,
}

/// A merged attribute value borrowed from the registry, with its layer.
struct LayeredValue<'a> {
    value: &'a str,
    layer: AttributeLayer,
    source: Option<&'a AttributeSource>,
}

/// Name of the attribute Asciidoctor sets to the folder of the converted document.
pub const DOCDIR: &str = "docdir";

/// A top-level document a file is converted as part of.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocRoot {
    document: PathBuf,
    /// The document directory of `document`, as a string: its `docdir`
    /// unless its config chain sets one.
    dir: String,
}

pub struct AttributeRegistry {
    builtin: HashMap<String, String>,
    /// Config chains by directory.
    config_chains: HashMap<PathBuf, HashMap<String, AttributeEntry>>,
    /// Config chains of the config files themselves, by config-file path:
    /// the chain of their directory without the file (see `config_chain`).
    config_file_chains: HashMap<PathBuf, HashMap<String, AttributeEntry>>,
    per_file: HashMap<PathBuf, HashMap<String, AttributeEntry>>,
    /// Document directories that differ from the file's parent: the folder of
    /// the symlink a document was opened through.
    doc_dirs: HashMap<PathBuf, PathBuf>,
    /// The top-level documents each file is converted as part of, sorted;
    /// they decide `docdir`. See [`Self::set_doc_roots`].
    doc_roots: HashMap<PathBuf, Vec<DocRoot>>,
    workspace_root: PathBuf,
}

impl AttributeRegistry {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let mut builtin = HashMap::new();
        // `docdir` of a file the registry has never seen.
        builtin.insert(DOCDIR.into(), root.display().to_string());
        builtin.insert("env-asciidoc-lsp".into(), String::new());
        Self {
            builtin,
            config_chains: HashMap::new(),
            config_file_chains: HashMap::new(),
            per_file: HashMap::new(),
            doc_dirs: HashMap::new(),
            doc_roots: HashMap::new(),
            workspace_root: root.to_path_buf(),
        }
    }

    /// Ensure a config chain is computed and cached for the given directory.
    ///
    /// Walks upward from `dir` as given, without resolving symlinks, until it
    /// reaches the workspace root (compared canonically), then merges the
    /// `.asciidoctorconfig` files from the root down to `dir`. A `dir` that
    /// never reaches the root gets no chain. The chain is cached under `dir`
    /// unchanged, so a symlinked folder keeps its own chain. Idempotent —
    /// returns early if already cached.
    pub fn ensure_config_for_dir(&mut self, dir: &Path) {
        if self.config_chains.contains_key(dir) || !dir.exists() {
            return;
        }
        let Ok(ws_root) = self.workspace_root.canonicalize() else {
            return;
        };

        // Collect directories from dir up to workspace_root (inclusive), then reverse
        let mut chain_dirs = Vec::new();
        let mut current = Some(dir);
        loop {
            let Some(here) = current else {
                return; // never reached the workspace root
            };
            chain_dirs.push(here.to_path_buf());
            if here.canonicalize().is_ok_and(|c| c == ws_root) {
                break;
            }
            current = here.parent();
        }
        chain_dirs.reverse(); // now root → dir order

        let mut merged = HashMap::new();
        merge_config_chain(&chain_dirs, None, &mut merged);
        self.config_chains.insert(dir.to_path_buf(), merged);

        // A config file in `dir` gets the chain without itself (see `config_chain`).
        for config_path in config_files(dir) {
            let mut own = HashMap::new();
            merge_config_chain(&chain_dirs, Some(&config_path), &mut own);
            self.config_file_chains.insert(config_path, own);
        }
    }

    /// The config chain `file` sees: the one of its document directory, or
    /// for a config file in that directory the chain without the file
    /// itself, as in `IntelliJ` (`!currentFile.equals(configFile)`).
    fn config_chain(&self, file: &Path) -> Option<&HashMap<String, AttributeEntry>> {
        let dir = self.doc_dir(file)?;
        match file.file_name().and_then(|name| name.to_str()) {
            Some(name) if CONFIG_FILE_NAMES.contains(&name) => {
                self.config_file_chains.get(&dir.join(name))
            }
            _ => self.config_chains.get(dir),
        }
    }

    /// Drop all cached config chains.
    ///
    /// Called when an `.asciidoctorconfig` file is created, changed, or deleted on
    /// disk. The next `ensure_config_for_dir()` call rebuilds chains lazily from
    /// the current state.
    pub fn clear_config_chains(&mut self) {
        self.config_chains.clear();
        self.config_file_chains.clear();
    }

    /// Drop the per-file attribute entry for `file`.
    ///
    /// Called when a `.adoc` file is deleted on disk so its document-header
    /// attributes no longer participate in `resolve()`. Crate-internal because
    /// the only caller is `Workspace::remove_file`.
    pub(crate) fn forget_file(&mut self, file: &Path) {
        self.per_file.remove(file);
        self.doc_dirs.remove(file);
        self.doc_roots.remove(file);
    }

    /// Record `dir` as the document directory of `file`.
    ///
    /// `file` is the registry key (canonical path); `dir` is the folder of
    /// the path the document is addressed by. For a symlinked document that
    /// is the link's folder: Asciidoctor's `docdir` keeps symlinks, so
    /// relative targets and the config chain start there. One directory per
    /// file: when the same file is open through a link and its real path,
    /// the last call wins for both.
    pub fn set_doc_dir(&mut self, file: &Path, dir: &Path) {
        if file.parent() == Some(dir) {
            self.doc_dirs.remove(file);
        } else {
            self.doc_dirs.insert(file.to_path_buf(), dir.to_path_buf());
        }
    }

    /// The directory relative paths in `file` resolve against: the one
    /// recorded by [`Self::set_doc_dir`], else `file`'s parent.
    #[must_use]
    pub fn doc_dir<'a>(&'a self, file: &'a Path) -> Option<&'a Path> {
        self.doc_dirs
            .get(file)
            .map(PathBuf::as_path)
            .or_else(|| file.parent())
    }

    fn doc_root(&self, document: &Path) -> DocRoot {
        DocRoot {
            document: document.to_path_buf(),
            dir: self
                .doc_dir(document)
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
        }
    }

    /// Record the top-level documents each file is converted as part of
    /// (sorted). Their folders are the file's `docdir`. A file missing from
    /// `roots` is its own top-level document. Returns the files whose
    /// `docdir` changed.
    pub fn set_doc_roots(&mut self, roots: &HashMap<PathBuf, Vec<PathBuf>>) -> Vec<PathBuf> {
        let files: HashSet<&PathBuf> = self.per_file.keys().chain(roots.keys()).collect();
        let next: HashMap<PathBuf, Vec<DocRoot>> = files
            .into_iter()
            .map(|file| {
                let documents = roots.get(file).map_or(&[][..], Vec::as_slice);
                let entry = if documents.is_empty() {
                    vec![self.doc_root(file)]
                } else {
                    documents.iter().map(|doc| self.doc_root(doc)).collect()
                };
                (file.clone(), entry)
            })
            .collect();
        let changed = next
            .iter()
            .filter(|(file, entry)| self.doc_roots.get(*file) != Some(*entry))
            .map(|(file, _)| file.clone())
            .collect();
        self.doc_roots = next;
        changed
    }

    /// Record `document` as the only top-level document of `file`, ahead of
    /// [`Self::set_doc_roots`]: for a graph walk from a known entry.
    pub(crate) fn assign_doc_root(&mut self, file: &Path, document: &Path) {
        let root = self.doc_root(document);
        self.doc_roots.insert(file.to_path_buf(), vec![root]);
    }

    /// `docdir` of `file`, once per top-level document it belongs to, in
    /// order. Asciidoctor sets it to the folder of the converted document
    /// and ignores `:docdir:` entries in the document, and so in any
    /// `.asciidoctorconfig` (prepended text counts as a header entry).
    fn docdir_layers(&self, file: &Path) -> Vec<LayeredValue<'_>> {
        let Some(roots) = self.doc_roots.get(file) else {
            return self
                .builtin
                .get(DOCDIR)
                .map(|dir| LayeredValue {
                    value: dir.as_str(),
                    layer: AttributeLayer::Builtin,
                    source: None,
                })
                .into_iter()
                .collect();
        };
        roots
            .iter()
            .map(|root| LayeredValue {
                value: root.dir.as_str(),
                layer: AttributeLayer::Builtin,
                source: None,
            })
            .collect()
    }

    /// Every distinct `docdir` of `file`, one per top-level document it
    /// belongs to; the first is the one [`Self::resolve`] returns.
    #[must_use]
    pub fn docdirs(&self, file: &Path) -> Vec<&str> {
        let mut dirs: Vec<&str> = Vec::new();
        for layered in self.docdir_layers(file) {
            if !dirs.contains(&layered.value) {
                dirs.push(layered.value);
            }
        }
        dirs
    }

    pub fn register_file_attributes(&mut self, file: &Path, attrs: &[AttributeDef]) {
        let is_own_root = self
            .doc_roots
            .get(file)
            .is_none_or(|roots| roots.len() == 1 && roots[0].document == file);
        if is_own_root {
            let root = self.doc_root(file);
            self.doc_roots.insert(file.to_path_buf(), vec![root]);
        }
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
        self.merged_layers(file)
            .into_iter()
            .map(|(name, layered)| (name, layered.value))
            .collect()
    }

    /// Merge builtin, config and per-file layers for `file`, later layers
    /// overriding earlier ones for the same name. Shared by
    /// `all_attributes()`, `known_attributes()` and `explain()`.
    fn merged_layers(&self, file: &Path) -> HashMap<&str, LayeredValue<'_>> {
        let mut merged: HashMap<&str, LayeredValue<'_>> = self
            .builtin
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str(),
                    LayeredValue {
                        value: v.as_str(),
                        layer: AttributeLayer::Builtin,
                        source: None,
                    },
                )
            })
            .collect();

        let config = self.config_chain(file);
        let layers = [
            (config, AttributeLayer::Config),
            (self.per_file.get(file), AttributeLayer::PerFile),
        ];
        for (entries, layer) in layers {
            for (k, entry) in entries.into_iter().flatten() {
                merged.insert(
                    k.as_str(),
                    LayeredValue {
                        value: entry.value.as_str(),
                        layer,
                        source: entry.source.as_ref(),
                    },
                );
            }
        }
        if let Some(docdir) = self.docdir_layers(file).into_iter().next() {
            merged.insert(DOCDIR, docdir);
        }
        merged
    }

    /// Resolve an attribute by name with priority: Document-Header > Config > Built-in.
    /// `docdir` follows [`Self::docdir_layers`] instead.
    #[must_use]
    pub fn resolve(&self, file: &Path, name: &str) -> Option<&str> {
        if name == DOCDIR {
            return self.docdir_layers(file).first().map(|l| l.value);
        }
        self.per_file
            .get(file)
            .and_then(|m| m.get(name))
            .map(|e| e.value.as_str())
            .or_else(|| self.config_chain(file)?.get(name).map(|e| e.value.as_str()))
            .or_else(|| self.builtin.get(name).map(std::string::String::as_str))
    }

    /// Resolve the source location of an attribute definition.
    /// Same priority as `resolve()`: `per_file` > config > builtin.
    /// Builtins have no source location and return None.
    #[must_use]
    pub fn resolve_source(&self, file: &Path, name: &str) -> Option<AttributeSource> {
        if name == DOCDIR {
            return self
                .docdir_layers(file)
                .first()
                .and_then(|l| l.source.cloned());
        }
        let per_file = self.per_file.get(file);
        let per_file_hit = per_file.and_then(|m| m.get(name));
        tracing::trace!(
            ?file,
            name,
            registered = per_file.is_some(),
            hit = per_file_hit.is_some(),
            "resolve_source: per_file layer"
        );
        if let Some(source) = per_file_hit.and_then(|e| e.source.clone()) {
            return Some(source);
        }

        let Some(dir) = self.doc_dir(file) else {
            tracing::trace!(
                ?file,
                name,
                "resolve_source: no parent dir, config layer skipped"
            );
            return None;
        };
        let chain = self.config_chain(file);
        let config_hit = chain.and_then(|c| c.get(name));
        tracing::trace!(
            ?dir,
            name,
            cached = chain.is_some(),
            hit = config_hit.is_some(),
            "resolve_source: config layer"
        );
        let source = config_hit.and_then(|e| e.source.clone());

        if source.is_none() {
            // builtins: no source → None
            tracing::trace!(
                name,
                hit = self.builtin.contains_key(name),
                "resolve_source: builtin layer (no source location)"
            );
        }
        source
    }

    /// Explain how `name` resolves for `file`, layer by layer, without
    /// normalising the path (same keys as `resolve()`).
    #[must_use]
    pub fn explain(&self, file: &Path, name: &str) -> AttributeExplanation {
        let file_attrs = self.per_file.get(file);
        let per_file = PerFileLookup {
            registered: file_attrs.is_some(),
            hit: file_attrs.and_then(|m| m.get(name)).map(AttributeHit::from),
        };

        let dir = self.doc_dir(file);
        let chain = self.config_chain(file);
        let config = ConfigLookup {
            dir: dir.map(Path::to_path_buf),
            chain_cached: chain.is_some(),
            hit: chain.and_then(|m| m.get(name)).map(AttributeHit::from),
        };

        let builtin = if name == DOCDIR {
            self.doc_roots
                .get(file)
                .and_then(|roots| roots.first())
                .map(|root| root.dir.clone())
                .or_else(|| self.builtin.get(name).cloned())
        } else {
            self.builtin.get(name).cloned()
        };

        let winner = self.merged_layers(file).get(name).map(|l| l.layer);

        AttributeExplanation {
            per_file,
            config,
            builtin,
            winner,
        }
    }

    /// All attributes visible for `file`, each with the layer that wins for it
    /// and its definition site, sorted by name. Same keys and priority as
    /// `all_attributes()`.
    #[must_use]
    pub fn known_attributes(&self, file: &Path) -> Vec<KnownAttribute> {
        let mut known: Vec<KnownAttribute> = self
            .merged_layers(file)
            .into_iter()
            .map(|(name, layered)| KnownAttribute {
                name: name.to_string(),
                value: layered.value.to_string(),
                layer: layered.layer,
                source: layered.source.cloned(),
            })
            .collect();
        known.sort_by(|a, b| a.name.cmp(&b.name));
        known
    }

    /// Substitute all `{attr}` references in a string using `resolve()`.
    #[must_use]
    pub fn substitute(&self, file: &Path, input: &str) -> String {
        substitute_inner(input, |name| self.resolve(file, name))
    }

    /// [`Self::substitute`] with `docdir` taken as `docdir`: the value for
    /// one particular top-level document (see [`Self::docdirs`]).
    #[must_use]
    pub fn substitute_with_docdir(&self, file: &Path, input: &str, docdir: &str) -> String {
        substitute_inner(input, |name| {
            if name == DOCDIR {
                Some(docdir)
            } else {
                self.resolve(file, name)
            }
        })
    }
}

impl From<&AttributeEntry> for AttributeHit {
    fn from(entry: &AttributeEntry) -> Self {
        Self {
            value: entry.value.clone(),
            source: entry.source.clone(),
        }
    }
}

/// Merge `.asciidoctorconfig` files from root to leaf directory into `merged`.
///
/// Walks `chain_dirs` (expected in root → leaf order), loading each config file
/// and merging its attributes. Later directories override earlier ones.
/// `exclude` contributes only its `{asciidoctorconfigdir}`, not its entries:
/// the chain of that config file itself.
fn merge_config_chain(
    chain_dirs: &[PathBuf],
    exclude: Option<&Path>,
    merged: &mut HashMap<String, AttributeEntry>,
) {
    for chain_dir in chain_dirs {
        for config_path in config_files(chain_dir) {
            let Ok(content) = std::fs::read_to_string(&config_path) else {
                continue;
            };
            // Synthetic attribute: {asciidoctorconfigdir} = directory of this config
            merged.insert(
                "asciidoctorconfigdir".into(),
                AttributeEntry {
                    value: chain_dir.display().to_string(),
                    source: Some(AttributeSource {
                        file: config_path.clone(),
                        line: 0,
                    }),
                },
            );
            if exclude == Some(config_path.as_path()) {
                continue;
            }

            for entry in parse_asciidoctorconfig(&content) {
                match entry {
                    // `docdir` is locked (see `docdir_layers`): setting it is skipped.
                    ConfigEntry::Set { name, .. } if name == DOCDIR => {}
                    ConfigEntry::Set { name, value, line } => {
                        let substituted = substitute_with(merged, &value);
                        merged.insert(
                            name,
                            AttributeEntry {
                                value: substituted,
                                source: Some(AttributeSource {
                                    file: config_path.clone(),
                                    line,
                                }),
                            },
                        );
                    }
                    ConfigEntry::Unset { name } => {
                        merged.remove(&name);
                    }
                }
            }
        }
    }
}

/// Substitute all `{attr}` references in a string using the provided attribute map.
/// Used by `ensure_config_for_dir()` for config-chain merging.
#[must_use]
fn substitute_with(attrs: &HashMap<String, AttributeEntry>, input: &str) -> String {
    substitute_inner(input, |name| attrs.get(name).map(|e| e.value.as_str()))
}

/// Common `{attr}` substitution logic. Walks the input character by character,
/// replacing `{name}` with the result of `lookup(name)`. Undefined attributes
/// are left as-is.
fn substitute_inner<'a>(input: &str, lookup: impl Fn(&str) -> Option<&'a str>) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars();

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
                if let Some(val) = lookup(&name) {
                    result.push_str(val);
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

/// File names of the config files in one folder, in load order.
///
/// Both load when present, as in the `IntelliJ` plugin: `.asciidoctorconfig.adoc`
/// first, then `.asciidoctorconfig`, so the file without extension wins for
/// the same attribute.
const CONFIG_FILE_NAMES: [&str; 2] = [".asciidoctorconfig.adoc", ".asciidoctorconfig"];

/// The config files present in a directory, in load order.
fn config_files(dir: &Path) -> Vec<PathBuf> {
    CONFIG_FILE_NAMES
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// One attribute entry of a config file.
#[derive(Debug)]
enum ConfigEntry {
    /// `:name: value`, with its 1-indexed line.
    Set {
        name: String,
        value: String,
        line: usize,
    },
    /// `:name!:` or `:!name:`.
    Unset { name: String },
}

/// Parse the attribute entries of an .asciidoctorconfig file.
fn parse_asciidoctorconfig(content: &str) -> Vec<ConfigEntry> {
    let mut result = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.starts_with(':') && line.len() > 1 {
            if let Some(rest) = line.strip_prefix(':') {
                if let Some((key, value)) = rest.split_once(':') {
                    let key = key.trim();
                    let value = value.trim();
                    let unset = key.strip_suffix('!').or_else(|| key.strip_prefix('!'));
                    if let Some(name) = unset.filter(|name| !name.is_empty()) {
                        result.push(ConfigEntry::Unset {
                            name: name.to_string(),
                        });
                    } else if !key.is_empty() {
                        result.push(ConfigEntry::Set {
                            name: key.to_string(),
                            value: value.to_string(),
                            line: idx + 1,
                        });
                    }
                }
            }
        }
    }
    result
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

    /// A relative path value stays as written: the config is prepended text,
    /// so it resolves where a document uses it, not against the config folder.
    #[test]
    fn test_config_relative_value_stays_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs = root.join("project").join("docs");
        fs::create_dir_all(&docs).unwrap();

        fs::write(docs.join(".asciidoctorconfig"), ":source-dir: ../src\n").unwrap();

        let mut registry = AttributeRegistry::new(&docs);
        registry.ensure_config_for_dir(&docs);

        let dummy = docs.join("any.adoc");
        assert_eq!(registry.resolve(&dummy, "source-dir"), Some("../src"));
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

    fn docdir_config(registry: &mut AttributeRegistry, dir: &str, value: &str) {
        let mut config_map = HashMap::new();
        config_map.insert(
            "docdir".into(),
            AttributeEntry {
                value: value.into(),
                source: Some(AttributeSource {
                    file: Path::new(dir).join(".asciidoctorconfig"),
                    line: 1,
                }),
            },
        );
        registry.config_chains.insert(dir.into(), config_map);
    }

    /// `docdir` is the folder of the top-level document, also in the files
    /// it includes. Asciidoctor locks it, so a `:docdir:` entry in a document
    /// is ignored, and so is one in any `.asciidoctorconfig` chain (prepended
    /// text counts as a header entry).
    #[test]
    fn test_docdir_priority() {
        let mut registry = AttributeRegistry::new(Path::new("/ws"));
        let book = Path::new("/ws/book/book.adoc");
        let part = Path::new("/ws/shared/part.adoc");
        registry.register_file_attributes(book, &[]);
        let header = AttributeDef {
            name: "docdir".into(),
            value: "/header".into(),
            line: 1,
        };
        registry.register_file_attributes(part, std::slice::from_ref(&header));

        // Before the include graph is known, a file is its own top-level document.
        assert_eq!(registry.resolve(part, "docdir"), Some("/ws/shared"));

        let roots = HashMap::from([
            (book.to_path_buf(), vec![book.to_path_buf()]),
            (part.to_path_buf(), vec![book.to_path_buf()]),
        ]);
        assert_eq!(registry.set_doc_roots(&roots), vec![part.to_path_buf()]);
        assert_eq!(registry.resolve(part, "docdir"), Some("/ws/book"));
        assert_eq!(registry.resolve(book, "docdir"), Some("/ws/book"));
        assert!(registry.resolve_source(part, "docdir").is_none());

        // The included file's own config chain does not count.
        docdir_config(&mut registry, "/ws/shared", "/shared-config");
        assert_eq!(registry.resolve(part, "docdir"), Some("/ws/book"));

        // Neither does the top-level document's chain.
        docdir_config(&mut registry, "/ws/book", "/configured");
        assert_eq!(registry.resolve(part, "docdir"), Some("/ws/book"));
        assert_eq!(registry.resolve(book, "docdir"), Some("/ws/book"));
        assert_eq!(
            registry.substitute(part, "{docdir}/x.adoc"),
            "/ws/book/x.adoc"
        );
        assert!(registry.resolve_source(part, "docdir").is_none());
        assert_eq!(
            registry.explain(part, "docdir").winner,
            Some(AttributeLayer::Builtin)
        );
        let known = registry.known_attributes(part);
        let docdir = known.iter().find(|k| k.name == "docdir").unwrap();
        assert_eq!(docdir.value, "/ws/book");
        assert_eq!(docdir.layer, AttributeLayer::Builtin);
    }

    /// A file several top-level documents include has one `docdir` per
    /// document, deduplicated; `resolve()` takes the first.
    #[test]
    fn test_docdirs_per_top_level_document() {
        let mut registry = AttributeRegistry::new(Path::new("/ws"));
        let a = Path::new("/ws/a/a.adoc");
        let a2 = Path::new("/ws/a/a2.adoc");
        let b = Path::new("/ws/b/b.adoc");
        let part = Path::new("/ws/shared/part.adoc");
        let roots = HashMap::from([(
            part.to_path_buf(),
            vec![a.to_path_buf(), a2.to_path_buf(), b.to_path_buf()],
        )]);
        registry.set_doc_roots(&roots);
        assert_eq!(registry.docdirs(part), vec!["/ws/a", "/ws/b"]);
        // A registered file no top-level document reaches (an include
        // cycle) is its own top-level document.
        let looped = Path::new("/ws/loop/x.adoc");
        registry.register_file_attributes(looped, &[]);
        registry.set_doc_roots(&roots);
        assert_eq!(registry.resolve(looped, "docdir"), Some("/ws/loop"));
        assert_eq!(registry.resolve(part, "docdir"), Some("/ws/a"));
        assert_eq!(
            registry.substitute_with_docdir(part, "{docdir}/x", "/ws/b"),
            "/ws/b/x"
        );
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
        // The value stays as written after substitution: `..` is popped only
        // where a path is used.
        assert_eq!(
            registry.resolve(&dummy, "source-dir"),
            Some(format!("{}/../src", docs.display())).as_deref()
        );
    }

    #[test]
    fn test_asciidoctorconfig_adoc_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs = root.join("project").join("docs");
        let src = root.join("project").join("src");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&src).unwrap();

        // Use .asciidoctorconfig.adoc (with extension) instead of .asciidoctorconfig
        fs::write(
            docs.join(".asciidoctorconfig.adoc"),
            ":source-dir: {asciidoctorconfigdir}/../src\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(&docs);
        registry.ensure_config_for_dir(&docs);

        let dummy = docs.join("any.adoc");

        assert_eq!(
            registry.resolve(&dummy, "asciidoctorconfigdir"),
            Some(docs.display().to_string()).as_deref(),
            ".asciidoctorconfig.adoc should be recognized"
        );
        assert_eq!(
            registry.resolve(&dummy, "source-dir"),
            Some(format!("{}/../src", docs.display())).as_deref(),
            "Attributes from .asciidoctorconfig.adoc should be resolved"
        );
    }

    #[test]
    fn test_asciidoctorconfig_plain_takes_priority_over_adoc() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(&root).unwrap();

        // Both files exist — plain should win
        fs::write(root.join(".asciidoctorconfig"), ":source: plain\n").unwrap();
        fs::write(root.join(".asciidoctorconfig.adoc"), ":source: adoc\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);

        let dummy = root.join("any.adoc");
        assert_eq!(
            registry.resolve(&dummy, "source"),
            Some("plain"),
            ".asciidoctorconfig (without extension) should take priority"
        );
    }

    /// Both config files in one folder load, as in IntelliJ:
    /// `.asciidoctorconfig.adoc` first, then `.asciidoctorconfig`, so the
    /// plain one wins for the same attribute (committed fixture
    /// `tests/fixtures/config_both_files`).
    #[test]
    fn test_both_config_files_in_folder_load_adoc_first() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/config_both_files")
            .canonicalize()
            .unwrap();
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let doc = root.join("doc.adoc");

        assert_eq!(registry.resolve(&doc, "shared"), Some("plain"));
        let shared = registry.resolve_source(&doc, "shared").unwrap();
        assert_eq!(
            (shared.file, shared.line),
            (root.join(".asciidoctorconfig"), 1)
        );
        assert_eq!(registry.resolve(&doc, "adoc-only"), Some("from-adoc"));
        let adoc_only = registry.resolve_source(&doc, "adoc-only").unwrap();
        assert_eq!(
            (adoc_only.file, adoc_only.line),
            (root.join(".asciidoctorconfig.adoc"), 2)
        );
        assert_eq!(registry.resolve(&doc, "derived"), Some("from-adoc/x"));
    }

    /// A case folder of the committed fixture `tests/fixtures/config_prepended_text`.
    fn prepended_text_case(case: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/config_prepended_text")
            .join(case)
            .canonicalize()
            .unwrap()
    }

    /// `:name!:` and `:!name:` unset an attribute, also one an earlier layer
    /// set, as in prepended AsciiDoc text.
    #[test]
    fn test_config_unset_removes_attribute() {
        let root = prepended_text_case("unset");
        let sub = root.join("sub");
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&sub);
        let doc = sub.join("doc.adoc");

        assert_eq!(registry.resolve(&doc, "gone-bang"), None);
        assert_eq!(registry.resolve(&doc, "gone-bang!"), None);
        assert_eq!(registry.resolve(&doc, "gone-prefix"), None);
        assert_eq!(registry.resolve(&doc, "!gone-prefix"), None);
        assert_eq!(registry.resolve(&doc, "local"), None);
        assert!(registry.resolve_source(&doc, "gone-bang").is_none());
        assert_eq!(registry.resolve(&doc, "kept"), Some("k"));
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

        assert_eq!(map.len(), 2);
        assert_eq!(map.get("docdir"), Some(&"/tmp"));
        assert_eq!(map.get("env-asciidoc-lsp"), Some(&""));
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

    /// Forgetting a deleted file also drops its document directory, so the
    /// file falls back to its own folder if it comes back.
    #[cfg(unix)]
    #[test]
    fn test_forget_file_drops_the_document_directory() {
        let project = crate::symlink_fixture().join("real/project");
        let file = project.join("book/twice.adoc");
        let mut registry = AttributeRegistry::new(&project);
        registry.set_doc_dir(&file, &project.join("config/book-link"));

        registry.forget_file(&file);

        assert_eq!(
            registry.doc_dir(&file),
            Some(project.join("book").as_path())
        );
    }

    /// A folder symlinked into the root from outside walks up through the
    /// link, not through its target, and reads the link folder's config.
    #[cfg(unix)]
    #[test]
    fn test_doc_dir_in_symlinked_folder_keeps_root_config_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":theme: dark\n").unwrap();
        fs::write(outside.join(".asciidoctorconfig"), ":local: yes\n").unwrap();
        let linked = root.join("linked");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        let file = outside.join("doc.adoc");

        let mut registry = AttributeRegistry::new(&root);
        registry.set_doc_dir(&file, &linked);
        registry.ensure_config_for_dir(&linked);

        assert_eq!(registry.resolve(&file, "theme"), Some("dark"));
        assert_eq!(registry.resolve(&file, "local"), Some("yes"));
        assert_eq!(registry.explain(&file, "theme").config.dir, Some(linked));
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

    /// The config chain ends at the workspace root, as in IntelliJ: an
    /// `.asciidoctorconfig` above it does not leak in (committed fixture
    /// `tests/fixtures/config_chain_root`, workspace root `project`).
    #[test]
    fn test_config_above_workspace_root_is_ignored() {
        let fx = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/config_chain_root")
            .canonicalize()
            .unwrap();
        let project = fx.join("project");

        let mut registry = AttributeRegistry::new(&project);
        registry.ensure_config_for_dir(&project);
        let file = project.join("doc.adoc");
        assert_eq!(registry.resolve(&file, "outside"), None);
        assert_eq!(registry.resolve(&file, "inside"), Some("project"));
    }

    // --- resolve_source() for asciidoctorconfigdir ---

    #[test]
    fn test_resolve_source_asciidoctorconfigdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);

        let dummy = root.join("any.adoc");
        let source = registry
            .resolve_source(&dummy, "asciidoctorconfigdir")
            .expect("asciidoctorconfigdir should have a source");
        assert_eq!(source.file, root.join(".asciidoctorconfig"));
        assert_eq!(source.line, 0, "synthetic attribute should use line 0");
    }

    // --- explain() tests (adoc-0137) ---

    #[test]
    fn test_explain_same_file_hit_wins_over_config_and_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        // `env-asciidoc-lsp` has all three layers; `docdir` ignores the
        // per-file one (see test_docdir_priority).
        fs::write(
            root.join(".asciidoctorconfig"),
            ":env-asciidoc-lsp: /config\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let file = root.join("doc.adoc");
        registry.register_file_attributes(
            &file,
            &[AttributeDef {
                name: "env-asciidoc-lsp".into(),
                value: "/header".into(),
                line: 3,
            }],
        );

        let ex = registry.explain(&file, "env-asciidoc-lsp");

        assert!(ex.per_file.registered);
        let hit = ex.per_file.hit.as_ref().expect("per-file hit");
        assert_eq!(hit.value, "/header");
        let src = hit.source.as_ref().expect("per-file source");
        assert_eq!(src.file, file);
        assert_eq!(src.line, 3);

        assert_eq!(ex.config.dir.as_deref(), Some(root.as_path()));
        assert!(ex.config.chain_cached);
        assert_eq!(
            ex.config.hit.as_ref().map(|h| h.value.as_str()),
            Some("/config")
        );
        assert_eq!(ex.builtin.as_deref(), Some(""));
        assert_eq!(ex.winner, Some(AttributeLayer::PerFile));
    }

    #[test]
    fn test_explain_config_chain_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let file = root.join("doc.adoc");
        registry.register_file_attributes(&file, &[]);

        let ex = registry.explain(&file, "imagesdir");

        assert!(ex.per_file.registered);
        assert!(ex.per_file.hit.is_none());
        assert!(ex.config.chain_cached);
        let hit = ex.config.hit.as_ref().expect("config hit");
        assert_eq!(hit.value, "images");
        let src = hit.source.as_ref().expect("config source");
        assert_eq!(src.file, root.join(".asciidoctorconfig"));
        assert_eq!(src.line, 1);
        assert!(ex.builtin.is_none());
        assert_eq!(ex.winner, Some(AttributeLayer::Config));
    }

    #[test]
    fn test_explain_builtin_hit() {
        let registry = AttributeRegistry::new(Path::new("/project"));
        let file = Path::new("/project/doc.adoc");

        let ex = registry.explain(file, "docdir");

        assert!(!ex.per_file.registered);
        assert!(ex.per_file.hit.is_none());
        assert_eq!(ex.config.dir.as_deref(), Some(Path::new("/project")));
        assert!(!ex.config.chain_cached);
        assert!(ex.config.hit.is_none());
        assert_eq!(ex.builtin.as_deref(), Some("/project"));
        assert_eq!(ex.winner, Some(AttributeLayer::Builtin));
    }

    #[test]
    fn test_explain_miss() {
        let registry = AttributeRegistry::new(Path::new("/project"));
        let file = Path::new("/project/doc.adoc");

        let ex = registry.explain(file, "nonexistent");

        assert!(ex.per_file.hit.is_none());
        assert!(ex.config.hit.is_none());
        assert!(ex.builtin.is_none());
        assert_eq!(ex.winner, None);
    }

    #[test]
    fn test_explain_winner_matches_resolve() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let file = root.join("doc.adoc");

        for name in ["imagesdir", "docdir", "missing"] {
            let ex = registry.explain(&file, name);
            let winning_value = match ex.winner {
                Some(AttributeLayer::PerFile) => ex.per_file.hit.map(|h| h.value),
                Some(AttributeLayer::Config) => ex.config.hit.map(|h| h.value),
                Some(AttributeLayer::Builtin) => ex.builtin,
                None => None,
            };
            assert_eq!(
                winning_value.as_deref(),
                registry.resolve(&file, name),
                "{name}"
            );
        }
    }

    /// Documents current behaviour (adoc-0137): the per-file layer is keyed by
    /// the exact path passed to `register_file_attributes`. A lookup through a
    /// symlinked path to the same directory does not find the registration.
    #[cfg(unix)]
    #[test]
    fn test_explain_symlinked_path_not_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let real = base.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut registry = AttributeRegistry::new(&real);
        let canonical_file = real.join("doc.adoc");
        registry.register_file_attributes(
            &canonical_file,
            &[AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 2,
            }],
        );

        let via_link = link.join("doc.adoc");
        let ex = registry.explain(&via_link, "author");

        assert!(
            !ex.per_file.registered,
            "raw symlinked path is not the registered key"
        );
        assert!(ex.per_file.hit.is_none());
        assert_eq!(ex.config.dir.as_deref(), Some(link.as_path()));
        assert!(!ex.config.chain_cached);
        assert_eq!(ex.winner, None);

        let ex_canonical = registry.explain(&canonical_file, "author");
        assert!(ex_canonical.per_file.registered);
        assert_eq!(ex_canonical.winner, Some(AttributeLayer::PerFile));
    }

    /// `known_attributes()` merges through `merged_layers()`, `resolve()` and
    /// `resolve_source()` walk the layers themselves: pin that they agree.
    #[test]
    fn test_known_attributes_agree_with_resolve_and_explain() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(
            root.join(".asciidoctorconfig"),
            ":imagesdir: images\n:toc: left\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let file = root.join("doc.adoc");
        registry.register_file_attributes(
            &file,
            &[
                AttributeDef {
                    name: "imagesdir".into(),
                    value: "img".into(),
                    line: 4,
                },
                AttributeDef {
                    name: "author".into(),
                    value: "Alice".into(),
                    line: 5,
                },
            ],
        );

        let known = registry.known_attributes(&file);
        let layer_of = |n: &str| known.iter().find(|a| a.name == n).expect(n).layer;
        assert_eq!(layer_of("imagesdir"), AttributeLayer::PerFile, "overridden");
        assert_eq!(layer_of("author"), AttributeLayer::PerFile);
        assert_eq!(layer_of("toc"), AttributeLayer::Config);
        assert_eq!(layer_of("docdir"), AttributeLayer::Builtin);

        for attr in &known {
            let name = attr.name.as_str();
            assert_eq!(
                Some(attr.value.as_str()),
                registry.resolve(&file, name),
                "{name}"
            );
            assert_eq!(
                attr.source.as_ref().map(|s| (s.file.clone(), s.line)),
                registry
                    .resolve_source(&file, name)
                    .map(|s| (s.file, s.line)),
                "{name}"
            );
            assert_eq!(
                Some(attr.layer),
                registry.explain(&file, name).winner,
                "{name}"
            );
        }
    }

    #[test]
    fn test_known_attributes_reports_winning_layer_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(
            root.join(".asciidoctorconfig"),
            ":imagesdir: images\n:toc: left\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let file = root.join("doc.adoc");
        registry.register_file_attributes(
            &file,
            &[AttributeDef {
                name: "imagesdir".into(),
                value: "img".into(),
                line: 4,
            }],
        );

        let known = registry.known_attributes(&file);
        let find = |n: &str| known.iter().find(|a| a.name == n).expect(n);

        let imagesdir = find("imagesdir");
        assert_eq!(imagesdir.value, "img");
        assert_eq!(imagesdir.layer, AttributeLayer::PerFile);
        let src = imagesdir.source.as_ref().unwrap();
        assert_eq!((src.file.as_path(), src.line), (file.as_path(), 4));

        let toc = find("toc");
        assert_eq!(toc.value, "left");
        assert_eq!(toc.layer, AttributeLayer::Config);
        let src = toc.source.as_ref().unwrap();
        assert_eq!(
            (src.file.as_path(), src.line),
            (root.join(".asciidoctorconfig").as_path(), 2)
        );

        let docdir = find("docdir");
        assert_eq!(docdir.layer, AttributeLayer::Builtin);
        assert!(docdir.source.is_none());

        assert_eq!(
            known.iter().filter(|a| a.name == "imagesdir").count(),
            1,
            "each name listed once, with its winning layer"
        );
        let names: Vec<&str> = known.iter().map(|a| a.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "sorted by name");
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
