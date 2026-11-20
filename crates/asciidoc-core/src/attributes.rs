use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::extractors::extract_conditionals;
use crate::types::{AttributeDef, ConditionalDirective, ConditionalKind, ConditionalOperator};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeSource {
    pub file: PathBuf,
    pub line: usize,
}

/// An attribute entry of a config file as written: where, and its value
/// before substitution (continued lines joined); `None` for an unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigAttribute {
    pub source: AttributeSource,
    pub raw: Option<String>,
}

impl ConfigAttribute {
    fn new(file: &Path, line: usize, raw: Option<&str>) -> Self {
        Self {
            source: AttributeSource {
                file: file.to_path_buf(),
                line,
            },
            raw: raw.map(str::to_string),
        }
    }
}

#[derive(Debug, Clone)]
struct AttributeEntry {
    value: String,
    source: Option<AttributeSource>,
}

/// A lookup layer of [`AttributeRegistry`] before a document's first line, in
/// descending priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttributeLayer {
    /// Merged `.asciidoctorconfig` chain cached for the file's document directory.
    Config,
    /// Built-in attributes (`docdir`, `env-asciidoc-lsp`).
    Builtin,
}

/// A value found in one layer, with its definition site when the layer has one.
#[derive(Debug, Clone)]
pub(crate) struct AttributeHit {
    pub value: String,
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
        let Some(chain_dirs) = self.config_dirs(dir) else {
            return;
        };

        let mut merged = HashMap::new();
        merge_config_chain(
            &chain_dirs,
            ChainLimit::All,
            &self.builtin,
            &mut merged,
            None,
        );
        self.config_chains.insert(dir.to_path_buf(), merged);

        // A config file in `dir` gets the chain without itself (see `config_chain`).
        for config_path in config_files(dir) {
            let mut own = HashMap::new();
            merge_config_chain(
                &chain_dirs,
                ChainLimit::Without(&config_path),
                &self.builtin,
                &mut own,
                None,
            );
            self.config_file_chains.insert(config_path, own);
        }
    }

    /// The directories from the workspace root down to `dir`, as the config
    /// chain of `dir` reads them. Walks upward from `dir` as given, without
    /// resolving symlinks, until it reaches the workspace root (compared
    /// canonically); `None` when it never does.
    fn config_dirs(&self, dir: &Path) -> Option<Vec<PathBuf>> {
        let ws_root = self.workspace_root.canonicalize().ok()?;
        let mut chain_dirs = Vec::new();
        let mut current = Some(dir);
        loop {
            let here = current?;
            chain_dirs.push(here.to_path_buf());
            if here.canonicalize().is_ok_and(|c| c == ws_root) {
                break;
            }
            current = here.parent();
        }
        chain_dirs.reverse(); // root → dir order
        Some(chain_dirs)
    }

    /// The last entry that sets or unsets `name` before the config file
    /// `file` in load order: the one the entry of `file` for `name`
    /// overrides. Never `file` itself or a config loaded after it. `None`
    /// when `file` is no config file or no entry before it names `name`.
    #[must_use]
    pub fn overridden_config_attribute(&self, file: &Path, name: &str) -> Option<ConfigAttribute> {
        let (dir, config_path) = self.config_file_location(file)?;
        let chain_dirs = self.config_dirs(dir)?;
        let mut watch = Watch {
            name: name.to_string(),
            last: None,
        };
        merge_config_chain(
            &chain_dirs,
            ChainLimit::Before(&config_path),
            &self.builtin,
            &mut HashMap::new(),
            Some(&mut watch),
        );
        watch.last
    }

    /// The config chain `file` sees: the one of its document directory, or
    /// for a config file in that directory the chain without the file
    /// itself, as in `IntelliJ` (`!currentFile.equals(configFile)`).
    fn config_chain(&self, file: &Path) -> Option<&HashMap<String, AttributeEntry>> {
        match self.config_file_location(file) {
            Some((_, config_path)) => self.config_file_chains.get(&config_path),
            None => self.config_chains.get(self.doc_dir(file)?),
        }
    }

    /// For a config file `file` (by its name), its document directory and
    /// its path there; `None` for any other file.
    fn config_file_location<'a>(&'a self, file: &'a Path) -> Option<(&'a Path, PathBuf)> {
        let name = file.file_name()?.to_str()?;
        if !CONFIG_FILE_NAMES.contains(&name) {
            return None;
        }
        let dir = self.doc_dir(file)?;
        Some((dir, dir.join(name)))
    }

    /// Whether `file` is a config file whose chain is known: one in the
    /// workspace, after [`Self::ensure_config_for_dir`].
    #[must_use]
    pub fn is_config_file(&self, file: &Path) -> bool {
        self.config_file_location(file)
            .is_some_and(|(_, config_path)| self.config_file_chains.contains_key(&config_path))
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
                .map_or_default(|dir| dir.display().to_string()),
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
    pub(crate) fn docdirs(&self, file: &Path) -> Vec<&str> {
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

    /// Resolve an attribute by name with priority: Document-Header > Config > Built-in.
    /// `docdir` follows [`Self::docdir_layers`] instead.
    #[must_use]
    pub(crate) fn resolve(&self, file: &Path, name: &str) -> Option<&str> {
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
    pub(crate) fn resolve_source(&self, file: &Path, name: &str) -> Option<AttributeSource> {
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

    /// `name` as Asciidoctor knows it before the first line of the top-level
    /// document `file`: from its config chain (prepended text), else built in.
    #[must_use]
    pub(crate) fn preset(&self, file: &Path, name: &str) -> Option<(AttributeLayer, AttributeHit)> {
        if name == DOCDIR {
            let layer = self.docdir_layers(file).into_iter().next()?;
            return Some((
                layer.layer,
                AttributeHit {
                    value: layer.value.to_string(),
                    source: layer.source.cloned(),
                },
            ));
        }
        if let Some(entry) = self.config_chain(file).and_then(|chain| chain.get(name)) {
            return Some((AttributeLayer::Config, AttributeHit::from(entry)));
        }
        self.builtin.get(name).map(|value| {
            (
                AttributeLayer::Builtin,
                AttributeHit {
                    value: value.clone(),
                    source: None,
                },
            )
        })
    }

    /// The names [`Self::preset`] knows for the top-level document `file`:
    /// builtins and its config chain, unordered.
    #[must_use]
    pub(crate) fn preset_names(&self, file: &Path) -> HashSet<&str> {
        let mut names: HashSet<&str> = self.builtin.keys().map(String::as_str).collect();
        names.extend(
            self.config_chain(file)
                .into_iter()
                .flatten()
                .map(|(name, _)| name.as_str()),
        );
        if !self.docdir_layers(file).is_empty() {
            names.insert(DOCDIR);
        }
        names
    }

    /// Substitute all `{attr}` references in a string using `resolve()`.
    #[must_use]
    pub(crate) fn substitute(&self, file: &Path, input: &str) -> String {
        substitute_attributes(input, |name| self.resolve(file, name))
    }

    /// [`Self::substitute`] with `docdir` taken as `docdir`: the value for
    /// one particular top-level document (see [`Self::docdirs`]).
    #[must_use]
    pub(crate) fn substitute_with_docdir(&self, file: &Path, input: &str, docdir: &str) -> String {
        substitute_attributes(input, |name| {
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
/// `limit` leaves a config file out of the chain, or ends it there. `watch`
/// notes the entries for one name on the way.
fn merge_config_chain(
    chain_dirs: &[PathBuf],
    limit: ChainLimit<'_>,
    builtin: &HashMap<String, String>,
    merged: &mut HashMap<String, AttributeEntry>,
    mut watch: Option<&mut Watch>,
) {
    for chain_dir in chain_dirs {
        for config_path in config_files(chain_dir) {
            if limit == ChainLimit::Before(&config_path) {
                return;
            }
            let Ok(content) = crate::source_text::read_adoc_file(&config_path) else {
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
            if limit == ChainLimit::Without(&config_path) {
                continue;
            }

            let mut read = ConfigRead {
                builtin,
                conditionals: config_path.extension().is_some_and(|ext| ext == "adoc"),
                max_depth: MAX_CONFIG_INCLUDE_DEPTH,
                stack: Vec::new(),
                watch: watch.as_deref_mut(),
            };
            read.apply_text(&config_path, &content, merged);
        }
    }
}

/// Which config files a chain reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainLimit<'a> {
    /// All of them.
    All,
    /// All but this file's entries: it contributes only its
    /// `{asciidoctorconfigdir}`, the chain of that config file itself.
    Without(&'a Path),
    /// Those loaded before this file.
    Before(&'a Path),
}

/// The last entry that sets or unsets `name` in a chain read.
struct Watch {
    name: String,
    last: Option<ConfigAttribute>,
}

/// Asciidoctor's default `max-include-depth`; the include graph sets no
/// limit of its own.
const MAX_CONFIG_INCLUDE_DEPTH: usize = 64;

/// Reading one config file, with the files it includes, into a chain.
struct ConfigRead<'a> {
    /// Built-in attributes: defined for `ifdef`/`ifndef`.
    builtin: &'a HashMap<String, String>,
    /// Whether `ifdef`/`ifndef` are evaluated: only in `.asciidoctorconfig.adoc`.
    /// Otherwise conditional lines are not read and block entries apply.
    conditionals: bool,
    /// How many levels includes nest below the config file at most.
    max_depth: usize,
    /// Canonical paths of the files being read, outermost first.
    stack: Vec<PathBuf>,
    /// The entries for one name, noted as they apply.
    watch: Option<&'a mut Watch>,
}

impl ConfigRead<'_> {
    /// Apply the entries of `content`, the text of `file`, to `merged` in
    /// order. Entries in a false `ifdef`/`ifndef` branch do not apply.
    fn apply_text(
        &mut self,
        file: &Path,
        content: &str,
        merged: &mut HashMap<String, AttributeEntry>,
    ) {
        self.stack
            .push(file.canonicalize().unwrap_or_else(|_| file.to_path_buf()));
        // One flag per open conditional block: whether its content applies.
        let mut blocks: Vec<bool> = Vec::new();
        for entry in parse_asciidoctorconfig(content) {
            let active = blocks.iter().all(|applies| *applies);
            match entry {
                ConfigEntry::Conditional {
                    directive,
                    single_line,
                } if self.conditionals => {
                    let holds = active && self.condition_holds(&directive, merged);
                    match single_line {
                        Some(entries) if holds => {
                            for entry in entries {
                                self.apply_entry(file, entry, merged);
                            }
                        }
                        Some(_) => {}
                        None => blocks.push(holds),
                    }
                }
                ConfigEntry::Ifeval if self.conditionals => blocks.push(true),
                ConfigEntry::Endif if self.conditionals => {
                    blocks.pop();
                }
                entry if active => self.apply_entry(file, entry, merged),
                _ => {}
            }
        }
        self.stack.pop();
    }

    /// Apply one attribute entry or include of `file`. `docdir` is locked
    /// (see [`AttributeRegistry::docdir_layers`]): setting it is skipped.
    fn apply_entry(
        &mut self,
        file: &Path,
        entry: ConfigEntry,
        merged: &mut HashMap<String, AttributeEntry>,
    ) {
        match entry {
            ConfigEntry::Set { name, .. } if name == DOCDIR => {}
            ConfigEntry::Set { name, value, line } => {
                self.note(file, &name, line, Some(&value));
                let substituted = substitute_with(merged, &value);
                merged.insert(
                    name,
                    AttributeEntry {
                        value: substituted,
                        source: Some(AttributeSource {
                            file: file.to_path_buf(),
                            line,
                        }),
                    },
                );
            }
            ConfigEntry::Unset { name, line } => {
                self.note(file, &name, line, None);
                merged.remove(&name);
            }
            ConfigEntry::Include { target } => self.include(file, &target, merged),
            ConfigEntry::Conditional { .. } | ConfigEntry::Ifeval | ConfigEntry::Endif => {}
        }
    }

    /// Note the entry for `name` on `line` of `file` if it is the one watched.
    fn note(&mut self, file: &Path, name: &str, line: usize, raw: Option<&str>) {
        if let Some(watch) = self.watch.as_deref_mut().filter(|watch| watch.name == name) {
            watch.last = Some(ConfigAttribute::new(file, line, raw));
        }
    }

    /// Read the entries of `target`, included from `file`, in place. The
    /// target resolves against the folder of `file`; a missing target, a
    /// file on the stack and one nested deeper than `max_depth` are skipped.
    fn include(&mut self, file: &Path, target: &str, merged: &mut HashMap<String, AttributeEntry>) {
        if self.stack.len() > self.max_depth {
            return;
        }
        let Some(dir) = file.parent() else {
            return;
        };
        let target = dir.join(substitute_with(merged, target));
        let Ok(canonical) = target.canonicalize() else {
            return;
        };
        if self.stack.contains(&canonical) {
            return;
        }
        let Ok(included) = crate::source_text::read_adoc_file(&target) else {
            return;
        };
        self.apply_text(&target, &included, merged);
    }

    /// Whether the content of `directive` applies, by Asciidoctor's rules:
    /// `,` needs any name, `+` all names defined (`ifndef` negates).
    fn condition_holds(
        &self,
        directive: &ConditionalDirective,
        merged: &HashMap<String, AttributeEntry>,
    ) -> bool {
        let defined = |name: &String| merged.contains_key(name) || self.builtin.contains_key(name);
        let names = &directive.attributes;
        let any = names.iter().any(defined);
        let all = names.iter().all(defined);
        match (directive.kind, directive.operator) {
            (ConditionalKind::Ifdef, ConditionalOperator::And) => all,
            (ConditionalKind::Ifdef, _) => any,
            (ConditionalKind::Ifndef, ConditionalOperator::And) => !all,
            (ConditionalKind::Ifndef, _) => !any,
        }
    }
}

/// Substitute all `{attr}` references in a string using the provided attribute map.
/// Used by `ensure_config_for_dir()` for config-chain merging.
#[must_use]
fn substitute_with(attrs: &HashMap<String, AttributeEntry>, input: &str) -> String {
    substitute_attributes(input, |name| attrs.get(name).map(|e| e.value.as_str()))
}

/// Common `{attr}` substitution logic. Walks the input character by character,
/// replacing `{name}` with the result of `lookup(name)`. Undefined attributes
/// are left as-is.
pub(crate) fn substitute_attributes<S: AsRef<str>>(
    input: &str,
    lookup: impl Fn(&str) -> Option<S>,
) -> String {
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
                    result.push_str(val.as_ref());
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
    /// `:name!:` or `:!name:`, with its 1-indexed line.
    Unset { name: String, line: usize },
    /// `include::target[...]`, target not yet substituted.
    Include { target: String },
    /// `ifdef::`/`ifndef::`: opens a block, or with `single_line` the
    /// entries of the single-line form `ifdef::name[:a: b]`.
    Conditional {
        directive: ConditionalDirective,
        single_line: Option<Vec<ConfigEntry>>,
    },
    /// `ifeval::[...]`: opens a block, not evaluated.
    Ifeval,
    /// `endif::...[]`: closes the innermost block.
    Endif,
}

/// Ends an attribute value that continues on the next line, as in Asciidoctor.
const LINE_CONTINUATION: &str = " \\";

/// Parse the attribute entries of an .asciidoctorconfig file.
fn parse_asciidoctorconfig(content: &str) -> Vec<ConfigEntry> {
    let mut result = Vec::new();
    let mut lines = content.lines().enumerate().peekable();
    while let Some((idx, line)) = lines.next() {
        let line = line.trim();
        if let Some(directive) = extract_conditionals(line).pop() {
            let text = line
                .split_once('[')
                .and_then(|(_, rest)| rest.strip_suffix(']'))
                .filter(|text| !text.is_empty());
            let single_line = text.map(|text| {
                let mut entries = parse_asciidoctorconfig(text);
                for entry in &mut entries {
                    if let ConfigEntry::Set { line, .. } | ConfigEntry::Unset { line, .. } = entry {
                        *line = idx + 1;
                    }
                }
                entries
            });
            result.push(ConfigEntry::Conditional {
                directive,
                single_line,
            });
        } else if line.starts_with("ifeval::[") && line.ends_with(']') {
            result.push(ConfigEntry::Ifeval);
        } else if line.starts_with("endif::") && line.ends_with(']') {
            result.push(ConfigEntry::Endif);
        } else if let Some(target) = include_target(line) {
            result.push(ConfigEntry::Include {
                target: target.to_string(),
            });
        } else if line.starts_with(':') && line.len() > 1 {
            if let Some(rest) = line.strip_prefix(':') {
                if let Some((key, value)) = rest.split_once(':') {
                    let key = key.trim();
                    let value = value.trim();
                    let unset = key.strip_suffix('!').or_else(|| key.strip_prefix('!'));
                    if let Some(name) = unset.filter(|name| !name.is_empty()) {
                        result.push(ConfigEntry::Unset {
                            name: name.to_string(),
                            line: idx + 1,
                        });
                    } else if !key.is_empty() {
                        let mut value = value.to_string();
                        if let Some(head) = value.strip_suffix(LINE_CONTINUATION) {
                            value = head.trim_end().to_string();
                            while let Some((_, next)) =
                                lines.next_if(|(_, next)| !next.trim().is_empty())
                            {
                                let next = next.trim();
                                let part = next.strip_suffix(LINE_CONTINUATION);
                                value.push(' ');
                                value.push_str(part.map_or(next, str::trim_end));
                                if part.is_none() {
                                    break;
                                }
                            }
                        }
                        result.push(ConfigEntry::Set {
                            name: key.to_string(),
                            value,
                            line: idx + 1,
                        });
                    }
                }
            }
        }
    }
    result
}

/// The entry for `name` on `line` (1-based) of the config file `file` with
/// the text `content`, as written: a continued value joined, an unset
/// without value. `None` when that line holds no entry for `name`.
#[must_use]
pub fn config_attribute_at(
    file: &Path,
    content: &str,
    line: usize,
    name: &str,
) -> Option<ConfigAttribute> {
    entry_at(file, &parse_asciidoctorconfig(content), line, name)
}

/// The entry for `name` on `line` among `entries`, looking into single-line
/// conditionals too.
fn entry_at(
    file: &Path,
    entries: &[ConfigEntry],
    line: usize,
    name: &str,
) -> Option<ConfigAttribute> {
    entries.iter().find_map(|entry| {
        let raw = match entry {
            ConfigEntry::Set {
                name: set,
                value,
                line: at,
            } if set == name && *at == line => Some(value.as_str()),
            ConfigEntry::Unset {
                name: unset,
                line: at,
            } if unset == name && *at == line => None,
            ConfigEntry::Conditional {
                single_line: Some(entries),
                ..
            } => return entry_at(file, entries, line, name),
            _ => return None,
        };
        Some(ConfigAttribute::new(file, line, raw))
    })
}

/// The target of an `include::target[...]` line; the attribute list is not read.
fn include_target(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("include::")?;
    let (target, _) = rest.split_once('[')?;
    (!target.is_empty() && rest.ends_with(']')).then_some(target)
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

    /// A BOM at the start of a config file or of a file it includes does not
    /// hide the first entry.
    #[test]
    fn test_config_and_config_include_with_bom_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(
            root.join(".asciidoctorconfig"),
            "\u{FEFF}:from-config: c\ninclude::attrs.adoc[]\n",
        )
        .unwrap();
        fs::write(root.join("attrs.adoc"), "\u{FEFF}:from-include: i\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);

        let doc = root.join("doc.adoc");
        assert_eq!(registry.resolve(&doc, "from-config"), Some("c"));
        assert_eq!(registry.resolve(&doc, "from-include"), Some("i"));
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

    fn attribute(file: &Path, line: usize, raw: Option<&str>) -> Option<ConfigAttribute> {
        Some(ConfigAttribute::new(file, line, raw))
    }

    /// What a config file overrides is what loads before it: root to leaf,
    /// per folder the `.adoc` file, then the plain one. Never itself, never
    /// what loads after it.
    #[test]
    fn test_overridden_config_attribute_follows_load_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join(".asciidoctorconfig.adoc"), ":d: adoc\n").unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":d: plain\n:e: e\n").unwrap();
        fs::write(sub.join(".asciidoctorconfig.adoc"), ":d: sub\n").unwrap();

        let registry = AttributeRegistry::new(&root);
        let root_adoc = root.join(".asciidoctorconfig.adoc");
        let root_plain = root.join(".asciidoctorconfig");
        let sub_adoc = sub.join(".asciidoctorconfig.adoc");
        assert_eq!(registry.overridden_config_attribute(&root_adoc, "d"), None);
        assert_eq!(
            registry.overridden_config_attribute(&root_plain, "d"),
            attribute(&root_adoc, 1, Some("adoc"))
        );
        assert_eq!(
            registry.overridden_config_attribute(&sub_adoc, "d"),
            attribute(&root_plain, 1, Some("plain"))
        );
        assert_eq!(
            registry.overridden_config_attribute(&sub_adoc, "e"),
            attribute(&root_plain, 2, Some("e"))
        );
        assert_eq!(
            registry.overridden_config_attribute(&sub_adoc, "none"),
            None
        );
        assert_eq!(
            registry.overridden_config_attribute(&sub.join("a.adoc"), "d"),
            None
        );
    }

    /// An unset is an entry too, and an entry of an included file has the
    /// included file as its place; the value is as written, not substituted.
    #[test]
    fn test_overridden_config_attribute_reports_unsets_and_included_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join("inc.adoc"), ":from-inc: {base}/i\n").unwrap();
        fs::write(
            root.join(".asciidoctorconfig"),
            ":base: b\n:d: 1\n:d!:\ninclude::inc.adoc[]\n",
        )
        .unwrap();
        fs::write(sub.join(".asciidoctorconfig"), ":d: 2\n").unwrap();

        let registry = AttributeRegistry::new(&root);
        let sub_config = sub.join(".asciidoctorconfig");
        assert_eq!(
            registry.overridden_config_attribute(&sub_config, "d"),
            attribute(&root.join(".asciidoctorconfig"), 3, None)
        );
        assert_eq!(
            registry.overridden_config_attribute(&sub_config, "from-inc"),
            attribute(&root.join("inc.adoc"), 1, Some("{base}/i"))
        );
    }

    /// The entry on a line is read as the chain reads it: a continued value
    /// joined, an unset without value.
    #[test]
    fn test_config_attribute_at_reads_continued_values_and_unsets() {
        let file = Path::new("/ws/.asciidoctorconfig");
        let content = ":long: one \\\n  two \\\nthree\n:gone!:\n";
        assert_eq!(
            config_attribute_at(file, content, 1, "long"),
            attribute(file, 1, Some("one two three"))
        );
        assert_eq!(
            config_attribute_at(file, content, 4, "gone"),
            attribute(file, 4, None)
        );
        assert_eq!(config_attribute_at(file, content, 2, "long"), None);
        assert_eq!(config_attribute_at(file, content, 1, "other"), None);
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
        let (layer, docdir) = registry.preset(part, "docdir").unwrap();
        assert_eq!(layer, AttributeLayer::Builtin);
        assert_eq!(docdir.value, "/ws/book");
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

    /// A value ending in ` \` continues on the next line: Asciidoctor joins
    /// the lines with one space, leading blanks of a continued line dropped.
    #[test]
    fn test_config_value_line_continuation() {
        let root = prepended_text_case("continuation");
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let doc = root.join("doc.adoc");

        assert_eq!(registry.resolve(&doc, "long"), Some("one two three"));
        assert_eq!(registry.resolve_source(&doc, "long").unwrap().line, 1);
        assert_eq!(registry.resolve(&doc, "after"), Some("next"));
        assert_eq!(registry.resolve_source(&doc, "after").unwrap().line, 4);
    }

    /// `include::` in a config reads the included file's entries in place,
    /// the target substituted and relative to the including file's folder.
    /// A missing target is skipped, an include cycle ends.
    #[test]
    fn test_config_include_reads_entries_in_place() {
        let root = prepended_text_case("include");
        let shared = root.join("shared");
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let doc = root.join("doc.adoc");

        assert_eq!(registry.resolve(&doc, "from-shared"), Some("s"));
        let source = registry.resolve_source(&doc, "from-shared").unwrap();
        assert_eq!((source.file, source.line), (shared.join("attrs.adoc"), 1));
        assert_eq!(registry.resolve(&doc, "before"), Some("shared"));
        assert_eq!(registry.resolve(&doc, "from-rel"), Some("r"));
        assert_eq!(registry.resolve(&doc, "from-nested"), Some("n"));
        assert_eq!(registry.resolve(&doc, "loop-a"), Some("a"));
        assert_eq!(registry.resolve(&doc, "loop-b"), Some("b"));
        assert_eq!(registry.resolve(&doc, "after-include"), Some("s"));
    }

    /// In `.asciidoctorconfig.adoc`, `ifdef`/`ifndef` (block and single-line
    /// form) decide against the attributes defined so far: builtins, earlier
    /// layers, earlier lines. Nothing in a false branch applies; `ifeval`
    /// is not evaluated, its entries apply.
    #[test]
    fn test_config_adoc_conditionals_are_evaluated() {
        let root = prepended_text_case("conditionals");
        let sub = root.join("sub");
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&sub);
        let doc = sub.join("doc.adoc");

        for applied in [
            "in-ifdef",
            "in-ifndef",
            "single-true",
            "from-builtin",
            "sees-parent",
            "after-nested",
            "any-of",
            "ifndef-all-of",
            "in-ifeval",
        ] {
            assert_eq!(
                registry.resolve(&doc, applied),
                Some("applied"),
                "{applied}"
            );
        }
        for skipped in [
            "in-false-ifdef",
            "single-false",
            "nested-in-false",
            "after-inner-endif",
            "all-of",
            "ifndef-any-of",
            "before-definition",
            "sees-dropped",
            "from-skipped-include",
        ] {
            assert_eq!(registry.resolve(&doc, skipped), None, "{skipped}");
        }
        assert_eq!(registry.resolve(&doc, "defined-early-kept"), Some("yes"));
        let source = registry.resolve_source(&doc, "single-true").unwrap();
        assert_eq!(
            (source.file, source.line),
            (sub.join(".asciidoctorconfig.adoc"), 11)
        );
    }

    /// In the plain `.asciidoctorconfig`, conditionals are not evaluated:
    /// block entries apply, single-line entries are not read.
    #[test]
    fn test_config_plain_conditionals_are_not_evaluated() {
        let root = prepended_text_case("conditionals_plain");
        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let doc = root.join("doc.adoc");

        assert_eq!(registry.resolve(&doc, "in-false-ifdef"), Some("applied"));
        assert_eq!(registry.resolve(&doc, "single-line"), None);
    }

    /// Includes nest at most `max_depth` levels below the config file.
    #[test]
    fn test_config_include_depth_is_limited() {
        let root = prepended_text_case("include");
        let config = root.join(".asciidoctorconfig");
        let content = fs::read_to_string(&config).unwrap();
        let mut merged = HashMap::new();
        merged.insert(
            "asciidoctorconfigdir".to_string(),
            AttributeEntry {
                value: root.display().to_string(),
                source: None,
            },
        );

        let builtin = HashMap::new();
        let mut read = ConfigRead {
            builtin: &builtin,
            conditionals: false,
            max_depth: 1,
            stack: Vec::new(),
            watch: None,
        };
        read.apply_text(&config, &content, &mut merged);

        assert!(merged.contains_key("from-rel"));
        assert!(!merged.contains_key("from-nested"));
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
        assert_eq!(registry.doc_dir(&file), Some(linked.as_path()));
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

    // --- layer priority ---

    #[test]
    fn test_same_file_hit_wins_over_config_and_builtin() {
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

        assert_eq!(registry.resolve(&file, "env-asciidoc-lsp"), Some("/header"));
        let src = registry
            .resolve_source(&file, "env-asciidoc-lsp")
            .expect("per-file source");
        assert_eq!(src.file, file);
        assert_eq!(src.line, 3);

        assert_eq!(registry.doc_dir(&file), Some(root.as_path()));
        let (layer, config) = registry.preset(&file, "env-asciidoc-lsp").unwrap();
        assert_eq!(layer, AttributeLayer::Config);
        assert_eq!(config.value, "/config");
    }

    #[test]
    fn test_config_chain_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();

        let mut registry = AttributeRegistry::new(&root);
        registry.ensure_config_for_dir(&root);
        let file = root.join("doc.adoc");
        registry.register_file_attributes(&file, &[]);

        assert_eq!(registry.resolve(&file, "imagesdir"), Some("images"));
        let src = registry
            .resolve_source(&file, "imagesdir")
            .expect("config source");
        assert_eq!(src.file, root.join(".asciidoctorconfig"));
        assert_eq!(src.line, 1);
        let (layer, hit) = registry.preset(&file, "imagesdir").unwrap();
        assert_eq!(layer, AttributeLayer::Config);
        assert_eq!(hit.value, "images");
    }

    #[test]
    fn test_docdir_builtin_hit() {
        let registry = AttributeRegistry::new(Path::new("/project"));
        let file = Path::new("/project/doc.adoc");

        assert_eq!(registry.resolve(file, "docdir"), Some("/project"));
        assert!(registry.resolve_source(file, "docdir").is_none());
        assert_eq!(registry.doc_dir(file), Some(Path::new("/project")));
        let (layer, hit) = registry.preset(file, "docdir").unwrap();
        assert_eq!(layer, AttributeLayer::Builtin);
        assert_eq!(hit.value, "/project");
        assert!(hit.source.is_none());
    }

    #[test]
    fn test_attribute_miss() {
        let registry = AttributeRegistry::new(Path::new("/project"));
        let file = Path::new("/project/doc.adoc");

        assert_eq!(registry.resolve(file, "nonexistent"), None);
        assert!(registry.resolve_source(file, "nonexistent").is_none());
        assert!(registry.preset(file, "nonexistent").is_none());
    }

    /// Documents current behaviour: the per-file layer is keyed by
    /// the exact path passed to `register_file_attributes`. A lookup through a
    /// symlinked path to the same directory does not find the registration.
    #[cfg(unix)]
    #[test]
    fn test_symlinked_path_not_registered() {
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
        assert_eq!(
            registry.resolve(&via_link, "author"),
            None,
            "raw symlinked path is not the registered key"
        );
        assert!(registry.resolve_source(&via_link, "author").is_none());
        assert_eq!(registry.doc_dir(&via_link), Some(link.as_path()));

        assert_eq!(registry.resolve(&canonical_file, "author"), Some("Alice"));
        let src = registry.resolve_source(&canonical_file, "author").unwrap();
        assert_eq!(
            (src.file.as_path(), src.line),
            (canonical_file.as_path(), 2)
        );
    }

    #[test]
    fn test_resolve_reports_winning_layer_and_source() {
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

        assert_eq!(registry.resolve(&file, "imagesdir"), Some("img"));
        let src = registry.resolve_source(&file, "imagesdir").unwrap();
        assert_eq!((src.file.as_path(), src.line), (file.as_path(), 4));
        let (layer, overridden) = registry.preset(&file, "imagesdir").unwrap();
        assert_eq!(layer, AttributeLayer::Config);
        assert_eq!(overridden.value, "images");

        assert_eq!(registry.resolve(&file, "toc"), Some("left"));
        let src = registry.resolve_source(&file, "toc").unwrap();
        assert_eq!(
            (src.file.as_path(), src.line),
            (root.join(".asciidoctorconfig").as_path(), 2)
        );

        let (layer, docdir) = registry.preset(&file, "docdir").unwrap();
        assert_eq!(layer, AttributeLayer::Builtin);
        assert!(docdir.source.is_none());
        assert!(registry.resolve_source(&file, "docdir").is_none());
    }

    // --- Config-Chain with LSP root_uri ---

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
