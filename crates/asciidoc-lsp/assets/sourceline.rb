# Asciidoctor extension for the preview: tags every block of the HTML with its
# source line and lists the files the document includes.
#
# - `data-line-N` is the line of the block in its own file.
# - `data-src-K` is added for blocks from an included file; K indexes the
#   file table in the page's `<script id="adoc-source-files">`.
#
# - Local image URLs become `files/<path below the workspace root>`, which the
#   preview server serves; the root comes in ASCIIDOC_LSP_ROOT.
#
# Loaded with `asciidoctor -r <this file>`.
require 'asciidoctor/extensions'
require 'json'
require 'stringio'

# Asciidoctor #4907: a setext (underline) title that spans an include boundary
# shifts the line numbers after it. Turning setext titles off avoids that.
Asciidoctor::Compliance.underline_style_section_titles = false

# Unsaved editor buffers of included files. The LSP names a JSON file
# `{absolute path: text}` in ASCIIDOC_LSP_OVERLAY; for those paths, Asciidoctor
# reads the buffer text instead of the file. This sits below the include
# directive on purpose: Asciidoctor's own reader still resolves the target,
# applies `leveloffset`, `tags`, `lines`, nesting and cycle checks, and
# numbers the lines. An IncludeProcessor would have to redo all of that.
module AdocBufferOverlay
  # Canonical path, like the LSP's keys: Asciidoctor does not resolve
  # symlinks. A file that exists only in an editor buffer keeps its
  # expanded path.
  def self.canonical(path)
    File.realpath(path)
  rescue SystemCallError
    File.expand_path(path)
  end

  TEXTS = begin
    path = ENV['ASCIIDOC_LSP_OVERLAY']
    raw = path ? JSON.parse(File.read(path)) : {}
    raw.each_with_object({}) { |(file, text), map| map[canonical(file)] = text }
  rescue StandardError
    {}
  end

  def self.text_for(path)
    return nil if TEXTS.empty? || !(String === path || path.respond_to?(:to_path))
    TEXTS[canonical(path.to_s)]
  end

  def file?(path)
    AdocBufferOverlay.text_for(path) ? true : super
  end

  def open(path, *args, **kwargs, &block)
    text = AdocBufferOverlay.text_for(path)
    return super unless text
    io = StringIO.new(text.dup.force_encoding(Encoding::UTF_8))
    block ? block.call(io) : io
  end
end

File.singleton_class.prepend AdocBufferOverlay unless AdocBufferOverlay::TEXTS.empty?

class SourceLinePreprocessor < Asciidoctor::Extensions::Preprocessor
  # The CLI has no --sourcemap in 2.0.x; the extension API turns it on.
  def process(doc, reader)
    doc.sourcemap = true
    reader
  end
end

class SourceLineTreeprocessor < Asciidoctor::Extensions::Treeprocessor
  def process(doc)
    docfile = doc.attr 'docfile'
    files = []
    doc.find_by(traverse_documents: true).each do |node|
      loc = node.source_location
      next unless loc && !node.is_a?(Asciidoctor::Document)
      file = loc.file.to_s
      role = "data-line-#{loc.lineno}"
      unless loc.file.nil? || file == docfile
        file = AdocBufferOverlay.canonical(file)
        idx = files.index(file) || (files << file).size - 1
        role = "data-src-#{idx} #{role}"
      end
      node.add_role role
    end
    doc.set_attr 'adoc-source-files', JSON.generate(files)
    nil
  end
end

class SourceFilesPostprocessor < Asciidoctor::Extensions::Postprocessor
  # The file table, index to path, as JSON next to the converted blocks.
  def process(doc, output)
    json = doc.attr('adoc-source-files') || '[]'
    # "</" must not end the script element early.
    json = json.gsub('</', '<\/')
    %(#{output}\n<script type="application/json" id="adoc-source-files">#{json}</script>\n)
  end
end

# Rewrites the `src` of every `<img>` that names a local file below the
# workspace root. The page lives at `/<token>/`, so `images/a.png` would
# resolve there and `../images/a.png` would drop the token; `files/...` is
# relative to the page and goes through the server's file route.
class ImageSrcPostprocessor < Asciidoctor::Extensions::Postprocessor
  IMG_SRC = /(<img\b[^>]*?\ssrc=")([^"]*)(")/
  SCHEME = /\A[a-zA-Z][a-zA-Z0-9+.\-]*:/

  def process(doc, output)
    root = ENV['ASCIIDOC_LSP_ROOT']
    return output if root.nil? || root.empty?
    roots = [File.expand_path(root), AdocBufferOverlay.canonical(root)].uniq
    base = File.expand_path(doc.base_dir)
    output.gsub(IMG_SRC) do
      head, src, tail = $1, $2, $3
      rel = relative_to_root(src, base, roots)
      rel ? "#{head}files/#{encode(rel)}#{tail}" : "#{head}#{src}#{tail}"
    end
  end

  private

  # The path of `src` below a root, or nil for URLs with a scheme, `//` URLs,
  # and files outside every root.
  def relative_to_root(src, base, roots)
    return nil if src.empty? || src.start_with?('//') || src.match?(SCHEME)
    # The converter escapes `&` in the attribute value.
    path = decode(src.gsub('&amp;', '&'))
    full = File.expand_path(path, base)
    candidates = [full, AdocBufferOverlay.canonical(full)].uniq
    roots.each do |root|
      candidates.each do |candidate|
        prefix = root.end_with?('/') ? root : "#{root}/"
        return candidate[prefix.size..] if candidate.start_with?(prefix)
      end
    end
    nil
  end

  def decode(text)
    text.gsub(/%([0-9A-Fa-f]{2})/) { $1.hex.chr }.force_encoding(Encoding::UTF_8)
  end

  def encode(path)
    path.b.gsub(%r{[^A-Za-z0-9\-._~/]}n) { |char| format('%%%02X', char.ord) }
  end
end

Asciidoctor::Extensions.register do
  preprocessor SourceLinePreprocessor
  treeprocessor SourceLineTreeprocessor
  postprocessor SourceFilesPostprocessor
  postprocessor ImageSrcPostprocessor
end
