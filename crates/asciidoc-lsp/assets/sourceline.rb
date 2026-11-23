# Asciidoctor extension for the preview: tags every block of the HTML with its
# source line and lists the files the document includes.
#
# - `data-line-N` is the line of the block in its own file.
# - `data-src-K` is added for blocks from an included file; K indexes the
#   file table in the page's `<script id="adoc-source-files">`.
# - `<script id="adoc-source-includes">` lists the includes as
#   `{parent, line, file}`: the directive stands on `line` (1-based) of file
#   `parent`, and pulls in `file`; both are file table indices, `parent` is
#   null for the main document.
#
# - Local image URLs become `files/<path below the workspace root>`, which the
#   preview server serves; the root comes in ASCIIDOC_LSP_ROOT. The output ends
#   with `<script id="adoc-image-files">`, a JSON array of those paths: the
#   server serves only files the shown document names there.
#
# - Embedded output drops what the standalone page does for `:toc: left|right`:
#   the toc div class (`toc-class`, default `toc2`) and the body classes
#   `<toc-class> toc-<toc-position>`. The toc div gets the class, and
#   `<script id="adoc-body-classes">` lists the body classes for the page,
#   `[]` when the standalone page would add none.
#
# - `<script id="adoc-page-title">` is the document title as a JSON string of
#   plain text, for the page's tab title; without a title, the file's base
#   name. It does not depend on `showtitle`.
#
# Loaded with `asciidoctor -r <this file>`.
require 'asciidoctor/extensions'
require 'cgi'
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

# Every include that Asciidoctor resolved: where the directive stood and what
# it pulled in. Skipped and unresolved includes never reach `push_include`.
module AdocIncludes
  LIST = []

  # Hooks the reader at the moment it enters an included file. Then `@file`
  # is still the including file, and the directive line was already consumed,
  # so `@lineno` is one past it.
  module Recorder
    def push_include(data, file = nil, path = nil, lineno = 1, attributes = {})
      LIST << { parent: @file, line: @lineno - 1, file: file } if file && @file
      super
    end
  end
end
Asciidoctor::PreprocessorReader.prepend AdocIncludes::Recorder

class SourceLinePreprocessor < Asciidoctor::Extensions::Preprocessor
  # The CLI has no --sourcemap in 2.0.x; the extension API turns it on.
  def process(doc, reader)
    doc.sourcemap = true
    AdocIncludes::LIST.clear
    reader
  end
end

class SourceLineTreeprocessor < Asciidoctor::Extensions::Treeprocessor
  # The `[file, line]` of `node`. Asciidoctor parses the content of a `> `
  # quote from a reader of its own, which knows no file and counts from 1:
  # such a block lies before the block that holds it, or has no file where
  # that block has one. It takes the line in the holder's file instead.
  def place(node, loc, places)
    holder = node.parent
    holder = holder.parent while holder && !places.key?(holder)
    return [loc.file, loc.lineno] unless holder
    file, line = places[holder]
    fresh = (loc.file.nil? && !file.nil?) || (loc.file == file && loc.lineno < line)
    return [loc.file, loc.lineno] unless fresh
    [file, line + loc.lineno - 1]
  end

  def process(doc)
    docfile = doc.attr 'docfile'
    files = []
    places = {}
    doc.find_by(traverse_documents: true).each do |node|
      loc = node.source_location
      next unless loc && !node.is_a?(Asciidoctor::Document)
      places[node] = place(node, loc, places)
      loc_file, lineno = places[node]
      file = loc_file.to_s
      role = "data-line-#{lineno}"
      unless loc_file.nil? || file == docfile
        file = AdocBufferOverlay.canonical(file)
        idx = files.index(file) || (files << file).size - 1
        role = "data-src-#{idx} #{role}"
      end
      node.add_role role
    end
    main = AdocBufferOverlay.canonical(docfile.to_s)
    # A file that rendered nothing still gets an index, so its include is
    # listed; the page finds no content for it and adds no anchor.
    index_of = lambda do |path|
      path = AdocBufferOverlay.canonical(path.to_s)
      path == main ? nil : (files.index(path) || (files << path).size - 1)
    end
    includes = AdocIncludes::LIST.map do |inc|
      { 'parent' => index_of.call(inc[:parent]), 'line' => inc[:line],
        'file' => index_of.call(inc[:file]) }
    end
    doc.set_attr 'adoc-source-files', JSON.generate(files)
    doc.set_attr 'adoc-source-includes', JSON.generate(includes)
    nil
  end
end

class SourceFilesPostprocessor < Asciidoctor::Extensions::Postprocessor
  # The file table, index to path, as JSON next to the converted blocks.
  def process(doc, output)
    files = (doc.attr('adoc-source-files') || '[]').gsub('</', '<\/')
    includes = (doc.attr('adoc-source-includes') || '[]').gsub('</', '<\/')
    # "</" must not end the script element early.
    %(#{output}\n<script type="application/json" id="adoc-source-files">#{files}</script>\n) +
      %(<script type="application/json" id="adoc-source-includes">#{includes}</script>\n)
  end
end

# The standalone page's body classes for a sidebar toc, which embedded output
# lacks (html5 converter, `convert_document`): the same condition decides.
# The toc div gets the `toc-class` the standalone page gives it; the embedded
# converter hardcodes `toc`. The list is JSON for the page, which sets them on
# its own body.
class TocLayoutPostprocessor < Asciidoctor::Extensions::Postprocessor
  TOC_DIV = /\A(<h1[^\n]*<\/h1>\n)?<div id="toc" class="toc">/

  def process(doc, output)
    classes = []
    if doc.sections? && doc.attr?('toc-class') && doc.attr?('toc') &&
       doc.attr?('toc-placement', 'auto')
      toc_class = doc.attr 'toc-class'
      escaped = toc_class.gsub('&', '&amp;').gsub('"', '&quot;').gsub('<', '&lt;').gsub('>', '&gt;')
      # Body classes only go with a rewritten div: a sidebar layout around an
      # inline toc would be worse than none.
      if output.match?(TOC_DIV)
        output = output.sub(TOC_DIV) { %(#{$1}<div id="toc" class="#{escaped}">) }
        classes = toc_class.split + ["toc-#{doc.attr 'toc-position', 'header'}"]
      end
    end
    # "</" must not end the script element early.
    json = JSON.generate(classes).gsub('</', '<\/')
    %(#{output}\n<script type="application/json" id="adoc-body-classes">#{json}</script>\n)
  end
end

# The document title as plain text: tags stripped, the entities that
# Asciidoctor writes decoded.
class PageTitlePostprocessor < Asciidoctor::Extensions::Postprocessor
  def process(doc, output)
    title = CGI.unescapeHTML(doc.doctitle(sanitize: true).to_s).strip
    title = doc.attr('docname').to_s if title.empty?
    # "</" must not end the script element early.
    json = JSON.generate(title).gsub('</', '<\/')
    %(#{output}\n<script type="application/json" id="adoc-page-title">#{json}</script>\n)
  end
end

# Rewrites the `src` of every `<img>` that names a local file below the
# workspace root. The page lives at `/<token>/`, so `images/a.png` would
# resolve there and `../images/a.png` would drop the token; `files/...` is
# relative to the page and goes through the server's file route.
class ImageSrcPostprocessor < Asciidoctor::Extensions::Postprocessor
  IMG_SRC = /(<img\b[^>]*?\ssrc=")([^"]*)(")/
  # At least two characters: `C:/` is a Windows path, not a scheme.
  SCHEME = /\A[a-zA-Z][a-zA-Z0-9+.\-]+:/

  def process(doc, output)
    root = ENV['ASCIIDOC_LSP_ROOT']
    return output if root.nil? || root.empty?
    roots = [File.expand_path(root), AdocBufferOverlay.canonical(root)].uniq
    base = File.expand_path(doc.base_dir)
    images = []
    rewritten = output.gsub(IMG_SRC) do
      head, src, tail = $1, $2, $3
      rel = relative_to_root(src, base, roots)
      images << rel if rel && rel.valid_encoding?
      rel ? "#{head}files/#{encode(rel)}#{tail}" : "#{head}#{src}#{tail}"
    end
    # Last in the output, so the server finds it by its end. "</" must not
    # end the script element early.
    json = JSON.generate(images.uniq).gsub('</', '<\/')
    %(#{rewritten}<script type="application/json" id="adoc-image-files">#{json}</script>\n)
  end

  private

  # The path of `src` below a root, or nil for URLs with a scheme, `//` URLs,
  # and files outside every root.
  def relative_to_root(src, base, roots)
    return nil if src.empty? || src.start_with?('//') || src.match?(SCHEME)
    # The converter escapes `&` in the attribute value.
    path = decode(src.gsub('&amp;', '&'))
    # `~` is no home folder in a URL; keep expand_path from reading it so.
    path = "./#{path}" if path.start_with?('~')
    full = File.expand_path(path, base)
    candidates = [full, AdocBufferOverlay.canonical(full)].uniq
    roots.each do |root|
      candidates.each do |candidate|
        prefix = root.end_with?('/') ? root : "#{root}/"
        return candidate[prefix.size..] if candidate.start_with?(prefix)
      end
    end
    nil
  rescue ArgumentError, EncodingError
    # One odd target must not fail the whole conversion.
    nil
  end

  def decode(text)
    text.b.gsub(/%([0-9A-Fa-f]{2})/n) { $1.hex.chr }.force_encoding(Encoding::UTF_8)
  end

  def encode(path)
    path.b.gsub(%r{[^A-Za-z0-9\-._~/]}n) { |char| format('%%%02X', char.ord) }
  end
end

Asciidoctor::Extensions.register do
  preprocessor SourceLinePreprocessor
  treeprocessor SourceLineTreeprocessor
  postprocessor SourceFilesPostprocessor
  postprocessor TocLayoutPostprocessor
  postprocessor PageTitlePostprocessor
  postprocessor ImageSrcPostprocessor
end
