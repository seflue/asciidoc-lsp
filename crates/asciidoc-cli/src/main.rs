use std::path::PathBuf;

use clap::Parser as ClapParser;

use asciidoc_core::types::IncludeTree;
use asciidoc_core::workspace::Workspace;

#[derive(ClapParser)]
#[command(name = "asciidoc-cli", about = "AsciiDoc Treesitter Parser PoC")]
struct Cli {
    /// Path to `AsciiDoc` file
    path: PathBuf,

    /// Show include tree
    #[arg(long)]
    tree: bool,

    /// Show all extracted information (sections, includes, xrefs, attributes, anchors)
    #[arg(long)]
    all: bool,

    /// Run diagnostics (broken xrefs, broken includes, duplicate anchors)
    #[arg(long)]
    diagnostics: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.diagnostics {
        let root = cli
            .path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let ws = Workspace::build(&root, &cli.path)?;

        if ws.diagnostics.is_empty() {
            println!("No diagnostics found.");
        } else {
            for d in &ws.diagnostics {
                println!("{}:{}: {}", d.file.display(), d.line, d.message);
            }
            std::process::exit(1);
        }
    } else if cli.tree {
        // tag::workspace-setup[]
        // Include-tree mode via Workspace -> Graph -> Tree
        let root = cli
            .path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let ws = Workspace::build(&root, &cli.path)?;
        let entry_canonical = cli.path.canonicalize()?;
        let tree = ws
            .include_graph
            .to_include_tree(&entry_canonical, &ws.documents);
        // end::workspace-setup[]
        print_include_tree(&tree, cli.all);
    } else {
        // Single-file mode: parse and extract everything
        let source = std::fs::read_to_string(&cli.path)?;
        let mut block_parser = asciidoc_core::parser::init_block_parser()?;
        let block_tree = asciidoc_core::parser::parse(&mut block_parser, &source)?;
        let root = block_tree.root_node();

        let sections = asciidoc_core::extractors::extract_sections(root, source.as_bytes());
        let includes = asciidoc_core::extractors::extract_includes(root, source.as_bytes());
        let attributes = asciidoc_core::extractors::extract_attributes(root, source.as_bytes());

        // Inline parsing for xrefs and anchors
        let inline_ranges = asciidoc_core::extractors::collect_inline_ranges(root);
        let (xrefs, anchors) = if inline_ranges.is_empty() {
            (vec![], vec![])
        } else {
            let mut inline_parser = asciidoc_core::parser::init_inline_parser()?;
            inline_parser
                .set_included_ranges(&inline_ranges)
                .map_err(|e| anyhow::anyhow!("Failed to set inline ranges: {e:?}"))?;
            let inline_tree = asciidoc_core::parser::parse(&mut inline_parser, &source)?;
            let inline_root = inline_tree.root_node();
            (
                asciidoc_core::extractors::extract_xrefs(inline_root, source.as_bytes()),
                asciidoc_core::extractors::extract_anchors(inline_root, source.as_bytes()),
            )
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
                            format!("{k}={v}")
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
                    .map(|d| format!(" \"{d}\""))
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

/// Print include tree with Unicode box-drawing characters
fn print_include_tree(tree: &IncludeTree, show_all: bool) {
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
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };

    for (i, child) in tree.includes.iter().enumerate() {
        let child_is_last = i == tree.includes.len() - 1;
        print_tree_child(child, &child_prefix, child_is_last, show_all);
    }
}

fn format_tree_node(tree: &IncludeTree, show_all: bool) -> String {
    let file_display = tree.file.file_name().map_or_else(
        || tree.file.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    );

    let section_info = if tree.sections.is_empty() {
        String::new()
    } else {
        format!(" ({} sections)", tree.sections.len())
    };

    let inline_info = if show_all {
        let parts: Vec<_> = [
            (!tree.xrefs.is_empty()).then(|| format!("{} xrefs", tree.xrefs.len())),
            (!tree.anchors.is_empty()).then(|| format!("{} anchors", tree.anchors.len())),
        ]
        .into_iter()
        .flatten()
        .collect();

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
        .map(|e| format!(" ⚠ {e}"))
        .unwrap_or_default();

    format!("{file_display}{section_info}{inline_info}{error_suffix}")
}
