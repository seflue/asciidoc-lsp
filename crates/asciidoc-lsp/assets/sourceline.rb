# Asciidoctor extension for the preview: tags every block of the HTML with its
# source line and lists the files the document includes.
#
# - `data-line-N` is the line of the block in its own file.
# - `data-src-K` is added for blocks from an included file; K indexes the
#   file table in the page's `<script id="adoc-source-files">`.
#
# Loaded with `asciidoctor -r <this file>`.
require 'asciidoctor/extensions'
require 'json'

# Asciidoctor #4907: a setext (underline) title that spans an include boundary
# shifts the line numbers after it. Turning setext titles off avoids that.
Asciidoctor::Compliance.underline_style_section_titles = false

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

Asciidoctor::Extensions.register do
  preprocessor SourceLinePreprocessor
  treeprocessor SourceLineTreeprocessor
  postprocessor SourceFilesPostprocessor
end
