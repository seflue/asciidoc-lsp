mod extractors;
mod parser;
mod types;
mod walker;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use clap::Parser as ClapParser;

use crate::types::IncludeTree;

#[derive(ClapParser)]
#[command(name = "asciidoc-lsp", about = "AsciiDoc Treesitter Parser PoC")]
struct Cli {
    /// Path to AsciiDoc file
    path: PathBuf,

    /// Show include tree
    #[arg(long)]
    tree: bool,

    /// Show all extracted information (sections, includes, xrefs, attributes, anchors)
    #[arg(long)]
    all: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.tree {
        // Include-tree mode: walk recursively from start file
        let attrs = load_default_attributes(&cli.path);
        let mut visited = HashSet::new();
        let tree = walker::walk_includes(&cli.path, &attrs, &mut visited);
        print_include_tree(&tree, "", true, cli.all);
    } else {
        // Single-file mode: parse and extract everything
        let source = std::fs::read_to_string(&cli.path)?;
        let mut block_parser = parser::init_block_parser()?;
        let block_tree = parser::parse(&mut block_parser, &source)?;
        let root = block_tree.root_node();

        let sections = extractors::extract_sections(root, source.as_bytes());
        let includes = extractors::extract_includes(root, source.as_bytes());
        let attributes = extractors::extract_attributes(root, source.as_bytes());

        // Inline parsing for xrefs and anchors
        let inline_ranges = extractors::collect_inline_ranges(root);
        let (xrefs, anchors) = if !inline_ranges.is_empty() {
            let mut inline_parser = parser::init_inline_parser()?;
            inline_parser
                .set_included_ranges(&inline_ranges)
                .map_err(|e| anyhow::anyhow!("Failed to set inline ranges: {:?}", e))?;
            let inline_tree = parser::parse(&mut inline_parser, &source)?;
            let inline_root = inline_tree.root_node();
            (
                extractors::extract_xrefs(inline_root, source.as_bytes()),
                extractors::extract_anchors(inline_root, source.as_bytes()),
            )
        } else {
            (vec![], vec![])
        };

        // Output
        println!("File: {}", cli.path.display());
        println!();

        println!("Sections ({}):", sections.len());
        for s in &sections {
            let indent = "  ".repeat(s.level);
            println!("  {}L{}: {} (line {})", indent, s.level, s.title, s.line);
        }
        println!();

        println!("Includes ({}):", includes.len());
        for inc in &includes {
            let attrs_str = if inc.attributes.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    inc.attributes
                        .iter()
                        .map(|(k, v)| if v.is_empty() {
                            k.clone()
                        } else {
                            format!("{}={}", k, v)
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            println!("  {} (line {}){}", inc.path, inc.line, attrs_str);
        }
        println!();

        println!("Attributes ({}):", attributes.len());
        for a in &attributes {
            println!("  :{}: {} (line {})", a.name, a.value, a.line);
        }
        println!();

        if cli.all {
            println!("Xrefs ({}):", xrefs.len());
            for x in &xrefs {
                let display = x
                    .display_text
                    .as_deref()
                    .map(|d| format!(" \"{}\"", d))
                    .unwrap_or_default();
                println!("  {} (line {}){}", x.target, x.line, display);
            }
            println!();

            println!("Anchors ({}):", anchors.len());
            for a in &anchors {
                println!("  [[{}]] (line {})", a.id, a.line);
            }
        }
    }

    Ok(())
}

/// Load attributes from .asciidoctorconfig files, searching upward from the
/// file's directory to the filesystem root (like .gitignore). Configs closer
/// to the file override values from parent directories.
fn load_default_attributes(file_path: &std::path::Path) -> HashMap<String, String> {
    let start_dir = match file_path.parent() {
        Some(dir) => match dir.canonicalize() {
            Ok(d) => d,
            Err(_) => return HashMap::new(),
        },
        None => return HashMap::new(),
    };

    // Collect config files from current dir upward (closest first)
    let mut config_contents = Vec::new();
    let mut current = Some(start_dir.as_path());
    while let Some(dir) = current {
        let config_path = dir.join(".asciidoctorconfig");
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            config_contents.push(content);
        }
        current = dir.parent();
    }

    // Merge from root to leaf (later entries = closer to file = override)
    let mut attrs = HashMap::new();
    for content in config_contents.iter().rev() {
        for (key, value) in parse_asciidoctorconfig(content) {
            attrs.insert(key, value);
        }
    }
    attrs
}

/// Parse :key: value lines from an .asciidoctorconfig file.
fn parse_asciidoctorconfig(content: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with(':') && line.len() > 1 {
            if let Some(rest) = line.strip_prefix(':') {
                if let Some((key, value)) = rest.split_once(':') {
                    let key = key.trim();
                    let value = value.trim();
                    if !key.is_empty() {
                        result.push((key.to_string(), value.to_string()));
                    }
                }
            }
        }
    }
    result
}

/// Print include tree with Unicode box-drawing characters
fn print_include_tree(tree: &IncludeTree, _prefix: &str, _is_last: bool, show_all: bool) {
    // Root node
    println!("{}", format_tree_node(tree, show_all));
    for (i, child) in tree.includes.iter().enumerate() {
        let is_last = i == tree.includes.len() - 1;
        print_tree_child(child, "", is_last, show_all);
    }
}

fn print_tree_child(tree: &IncludeTree, prefix: &str, is_last: bool, show_all: bool) {
    let connector = if is_last { "└── " } else { "├── " };
    println!(
        "{}{}{}",
        prefix,
        connector,
        format_tree_node(tree, show_all)
    );

    let child_prefix = if is_last {
        format!("{}    ", prefix)
    } else {
        format!("{}│   ", prefix)
    };

    for (i, child) in tree.includes.iter().enumerate() {
        let child_is_last = i == tree.includes.len() - 1;
        print_tree_child(child, &child_prefix, child_is_last, show_all);
    }
}

fn format_tree_node(tree: &IncludeTree, show_all: bool) -> String {
    let file_display = tree
        .file
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| tree.file.display().to_string());

    let section_info = if !tree.sections.is_empty() {
        format!(" ({} sections)", tree.sections.len())
    } else {
        String::new()
    };

    let inline_info = if show_all {
        let mut parts = Vec::new();
        if !tree.xrefs.is_empty() {
            parts.push(format!("{} xrefs", tree.xrefs.len()));
        }
        if !tree.anchors.is_empty() {
            parts.push(format!("{} anchors", tree.anchors.len()));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", parts.join(", "))
        }
    } else {
        String::new()
    };

    let error_suffix = tree
        .error
        .as_ref()
        .map(|e| format!(" ⚠ {}", e))
        .unwrap_or_default();

    format!(
        "{}{}{}{}",
        file_display, section_info, inline_info, error_suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_asciidoctorconfig() {
        let content = ":imagesdir: images\n:source-dir: ../src\n:toc:\n\nNot an attribute line\n";
        let result = parse_asciidoctorconfig(content);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], ("imagesdir".into(), "images".into()));
        assert_eq!(result[1], ("source-dir".into(), "../src".into()));
        assert_eq!(result[2], ("toc".into(), "".into()));
    }

    #[test]
    fn test_parse_asciidoctorconfig_empty() {
        let result = parse_asciidoctorconfig("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_default_attributes_hierarchy() {
        // Create temp dir structure: parent/child/
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        // Parent config
        fs::write(
            parent.join(".asciidoctorconfig"),
            ":imagesdir: images\n:chapdir: chapters\n",
        )
        .unwrap();

        // Child config overrides imagesdir
        fs::write(child.join(".asciidoctorconfig"), ":imagesdir: img\n").unwrap();

        // Fake file in child dir
        let file_path = child.join("test.adoc");
        fs::write(&file_path, "").unwrap();

        let attrs = load_default_attributes(&file_path);
        // Child overrides parent
        assert_eq!(attrs.get("imagesdir").unwrap(), "img");
        // Parent value inherited
        assert_eq!(attrs.get("chapdir").unwrap(), "chapters");
    }
}
