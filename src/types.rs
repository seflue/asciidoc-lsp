use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Section {
    pub level: usize,
    pub title: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct IncludeMacro {
    pub path: String,
    pub attributes: Vec<(String, String)>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Xref {
    pub target: String,
    pub display_text: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct AttributeDef {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Anchor {
    pub id: String,
    pub line: usize,
}

#[derive(Debug)]
pub struct IncludeTree {
    pub file: PathBuf,
    pub includes: Vec<IncludeTree>,
    pub sections: Vec<Section>,
    pub xrefs: Vec<Xref>,
    pub anchors: Vec<Anchor>,
    pub error: Option<String>,
}
