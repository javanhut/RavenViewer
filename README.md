# Raven Viewer

A fast, free document reader and writer for Raven Linux. Open a PDF, a Word
document (`.docx` or Word 97–2003 `.doc`) or a text file and read it, mark
it up, edit it, convert it, or combine several into one. Start a new
document, type into it, paste pictures in, and save it as DOCX, DOC, PDF or
text — no browser, no account, nothing to pay for, and no office suite
installed alongside: everything is done by Raven Viewer itself.

## What it does

| | |
|---|---|
| **PDF** | Continuous pages, rendered in pure Rust ([hayro](https://github.com/LaurenzV/hayro)) on background threads; only pages near the viewport are rasterized, so a 700-page book opens in ~0.2s |
| **Jumping** | Fling the scrollbar or jump to a chapter and the page you land on is drawn first — work for pages you scrolled past is dropped, not queued |
| **Go to page** | Click the page counter or `Ctrl+G`; `Ctrl+Home` / `Ctrl+End` for the ends |
| **Contents** | The document outline in the sidebar; click a chapter to jump |
| **Notes** | Annotations (comments, highlights, stamps…) listed with their page; click to jump to the spot |
| **Search** | `Ctrl+F`, Enter for next, Shift+Enter for previous. Pages are searched lazily from where you are and stop at the first hit; tolerant of lost word spaces and ligatures |
| **Zoom** | Fit width by default; `Ctrl` + scroll, pinch, `Ctrl++` / `Ctrl+−`, `Ctrl+0` to re-fit. Word and text documents zoom too (`Ctrl+0` back to actual size). Zoomed wider than the window, a page is scrolled sideways, with a scrollbar that stays visible (or Shift + scroll). Past the point where a whole page is a sensible thing to draw, PDF pages are tiled and only the tiles on screen are rasterized, so 800% costs a screenful rather than a page |
| **Tabs** | Every document opens in a tab of its own (`Ctrl+Tab` / `Alt+1…9` to switch, `Ctrl+W` to close; unsaved changes are asked about). Opening a file that is already open shows its tab. Files opened from the file manager go into the window in use |
| **Split view** | `Ctrl+\` or the two-pane button: two documents side by side, each with its own row of tabs. The side last clicked is the one the header, the menu and the shortcuts act on (its tabs are underlined). Drag tabs between the sides, or right-click a tab → Move to Other Side |
| **Windows** | Menu → New Window (`Ctrl+Shift+N`); running `raven-viewer` again opens another window; `raven-viewer --new-window` starts a separate instance |
| **Dark pages** | Menu → Dark Pages, for reading at night |
| **Resume** | Reopening a file returns to the page you were on |
| **Select & copy** | Drag across PDF text, double-click a word, triple-click a line, Shift+click to extend; `Ctrl+C` copies. Works on OCR'd scans too, and on pages turned sideways |
| **Mark up PDFs** | Highlight (four colours), underline or strike out the selection; add sticky notes and text boxes; edit or delete them from the page's right-click menu or the Notes sidebar. Written as standard annotations with appearance streams, so every reader shows them |
| **Pages** | Right-click a page (or Menu → Page) to rotate it, move it up or down, or delete it |
| **Undo & save** | `Ctrl+Z` / `Ctrl+Shift+Z` step through PDF edits; `Ctrl+S` saves, `Ctrl+Shift+S` saves a copy. Closing or opening another file with unsaved changes asks first |
| **Word documents** | `.docx` and Word 97–2003 `.doc` open as a clean reading view: headings, emphasis, alignment, lists, tables, quotes and pictures |
| **Edit** | `Ctrl+E` or the pencil: type into the document, bold / italic / underline / highlight, and headings, quotes and bullets from the formatting bar. Paragraphs you don't touch are saved byte for byte, so fields and styles survive; paragraphs holding equations, fields or charts are shown read-only rather than risk them |
| **Pictures** | Shown in the text. Insert Picture… (formatting bar or right-click), paste one (`Ctrl+V` — a screenshot, a picture copied from a browser, or picture files copied in the file manager), or drop picture files onto the page. They can be typed around, cut, copied, pasted and deleted like a character, and undo brings them back |
| **New documents** | `Ctrl+N` or Menu → New Document: a blank page with Word's standard styles, on Letter or A4 paper by your locale. Save asks where it goes |
| **Text files** | `.txt` (and `.md`) open in the same editor; saving writes plain text back |
| **Save in any format** | Save writes the format the file came in. Save As (`Ctrl+Shift+S`) writes the format the name ends in: `.docx`, `.doc`, `.pdf` or `.txt`. Writing over a `.doc` asks first, since headers, footers, notes and comments aren't kept |
| **Export as PDF** | `Ctrl+Shift+E`: the document set on its own paper size and margins, in its fonts, with pictures and tables, headings as the PDF's bookmarks and selectable text. The document itself stays as it was |
| **Undo** | `Ctrl+Z` / `Ctrl+Shift+Z` take back typing a word at a time, formatting, and pictures |
| **Combine** | Menu → Combine Files…: pick files, order them, save. PDFs keep every file's bookmarks under an entry per file; Word and text files follow one another on new pages, bringing their images, lists and styles; a mix of PDFs and documents becomes one PDF |
| **Look** | GTK 4 + libadwaita with Raven Glass; theme, accent and transparency follow `~/.config/raven/desktop.toml` |

## Use

```sh
raven-viewer book.pdf            # open a file (as a tab, if Raven Viewer is running)
raven-viewer open notes.docx     # same, `open` is optional
raven-viewer                     # another window: Open… or drag a file in
raven-viewer --new-window a.pdf  # a separate instance of its own
raven-viewer new letter.docx     # write an empty document (.docx, .doc, .txt)
raven-viewer convert report.doc report.pdf   # or .docx, .doc, .txt — by OUT's name
raven-viewer combine out.pdf a.pdf b.docx notes.txt   # join files without a window
raven-viewer set-default         # make it the default for PDF and Word files
```

A second `raven-viewer FILE` hands the file to the running instance, so
opening from the file manager or a browser download is instant.

### Keyboard

| Action | Keys |
|---|---|
| Open (in a new tab) | `Ctrl+O` |
| New document | `Ctrl+N` |
| New window | `Ctrl+Shift+N` |
| Split view | `Ctrl+\` |
| Next / previous tab | `Ctrl+Tab` / `Ctrl+Shift+Tab`, `Alt+1…9` |
| Save / Save As | `Ctrl+S` / `Ctrl+Shift+S` |
| Export as PDF | `Ctrl+Shift+E` |
| Find | `Ctrl+F` |
| Copy selection | `Ctrl+C` |
| Select the page's text | `Ctrl+A` |
| Highlight selection | `Ctrl+H` |
| Undo / redo | `Ctrl+Z` / `Ctrl+Shift+Z` |
| Edit a document | `Ctrl+E`, then `Ctrl+B` / `Ctrl+I` / `Ctrl+U` |
| Paste a picture | `Ctrl+V` (or drop a picture file) |
| Zoom in / out / fit | `Ctrl++` / `Ctrl+−` / `Ctrl+0` |
| Next / previous page | `N` / `P` (when not typing), `Ctrl+PgDn` / `Ctrl+PgUp` |
| Go to page | `Ctrl+G` |
| First / last page | `Ctrl+Home` / `Ctrl+End` |
| Sidebar | `F9` |
| Shortcuts | `Ctrl+?` |
| Close tab / window | `Ctrl+W` / `Ctrl+Shift+W` |

## Build

```sh
make            # release build
make run FILE=book.pdf
sudo make install && raven-viewer set-default
```

or with ImLazy: `imlazy build`, `imlazy dev -- book.pdf`, `imlazy install`.

No C toolchain beyond what GTK already needs, no libclang. PDF export uses
the Pango and Cairo that GTK already brings; no other application is needed
at run time. The first build
compiles the dependencies once (slow on small machines — ~25 min on 4
cores); after that a change to Raven Viewer rebuilds in a few seconds.
Dependencies are always optimized, even in debug builds, so `cargo run` is
as smooth as a release build.

## Configuration

`~/.config/raven/viewer.toml`, written by the app:

```toml
show_sidebar = true
dark_pages = false
remember_position = true
highlight_color = "yellow"   # yellow, green, blue or pink
```

## Tests

```sh
cargo test
# engine timings and a PNG of page 1 on any PDF:
RAVEN_TEST_PDF=book.pdf RAVEN_TEST_QUERY="word" cargo test smoke -- --ignored --nocapture
# tabs, split view, zoom with sideways scrolling, windows:
GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 RAVEN_DRIVE_DIR=/tmp/shots \
  RAVEN_DRIVE_PDF=a.pdf RAVEN_DRIVE_DOCX=b.docx cargo test drive_tabs -- --ignored --nocapture
# the selectable text of a page, as it would be copied:
RAVEN_TEST_PDF=book.pdf RAVEN_TEST_PAGE=30 cargo test text_layer -- --ignored --nocapture
# drive the real window offscreen (open, select, copy, highlight, rotate,
# undo, save; then edit and save a DOCX), a PNG per step:
gtk4-broadwayd :7 & GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 RAVEN_DRIVE_DIR=/tmp/shots \
  RAVEN_DRIVE_PDF=a.pdf RAVEN_DRIVE_DOCX=b.docx cargo test drive_the_window -- --ignored --nocapture
# a new document with pictures (from a file and pasted), saved in every
# format, then .doc and text files opened and edited; FIXTURES holds
# red.png, sample.doc and notes.txt:
GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 RAVEN_DRIVE_DIR=/tmp/shots \
  RAVEN_DRIVE_FIXTURES=fixtures cargo test drive_documents -- --ignored --nocapture
# what a real .doc reads as (and it written back as DOCX, PDF and .doc):
RAVEN_TEST_DOC=a.doc cargo test doc_real -- --ignored --nocapture
```

GTK tests run on one shared GTK thread (`gtk_test::run`), so they run
rather than quietly skipping when the harness puts them on another thread.

## How text selection works

`lopdf` can say what words a page has but not where they are. hayro's
interpreter hands every glyph it draws to a `Device` with its Unicode value
and transform, so `pdftext` interprets the page into a device that keeps
those instead of painting them: each character gets a box, lines are the
runs that share a baseline, and word spaces a producer only implied by
positioning are put back when copying. Hit-testing happens in a frame turned
to the text's direction, so a sideways page selects as naturally as an
upright one. Pages are read on their own thread as they come on screen,
~3 ms each.

## How editing stays safe

A PDF edit goes to a thread that holds the parsed document, and comes back
as the whole new file. The view swaps it in under the pages it is already
showing — the old pixels stay until the new ones land, so nothing flashes —
and the previous file goes on the undo stack. Highlights are drawn
translucent rather than with the Multiply blend Acrobat uses: hayro renders
a page as an isolated group, where Multiply paints the text over black.

A DOCX paragraph keeps an invisible mark naming the paragraph it came from.
On save, unchanged paragraphs are copied from the original XML, edited ones
are rewritten keeping their paragraph and run properties (alignment, fonts,
sizes), and new ones use the document's own styles — or direct formatting
when it has none, because LibreOffice drops the numbering of a paragraph
that names a missing style.

## How the formats are handled

A **DOCX** is a zip of XML. A picture in it is a `<w:drawing>` naming a part
of the package; its paragraph stays editable, and the drawing is written
back as it was wherever the paragraph ends up (renumbered, so a copied
picture never shares an id). A picture added in Raven Viewer becomes a new
part of the package with its relationship and content type.

A **Word 97–2003 `.doc`** is a compound file (`cfb.rs` reads and writes the
container). `doc.rs` reads the File Information Block, the piece table, the
character and paragraph property pages, the styles, the fonts, the page
setup and the inline pictures in the Data stream, into the same paragraphs
and runs a DOCX is read into; the editor never knows the difference. Saving
writes a Word 97 file from scratch with the same parts — text, styles,
formatting, tables, pictures, page setup — so headers, footers, notes and
comments in a `.doc` you overwrite are not kept, which is why Raven Viewer
asks first. Word 6 and Word 95 files, and password-protected ones, are
refused with a message rather than misread.

A **text file** is read as UTF-8, UTF-16 (by its byte order mark) or
Windows-1252, one paragraph per line, and written back as UTF-8.

**PDF export** (`render.rs`) lays the document out with Pango and draws it
with Cairo's PDF surface: the document's paper and margins, its default
font and size, Word's standard heading sizes, lists, tables and pictures
(JPEGs are embedded as they are). Line by line pagination keeps headings
with the text after them. Headings become the PDF's bookmarks.

**Undo** (`history.rs`) records every change to the editor's buffer — text,
pictures, formatting, and the marks that tie paragraphs to the file — and
plays it back exactly. GTK's own undo keeps text only: it cannot bring a
deleted picture back, and an inserted one would put its later offsets out.

## How it stays fast on long books

Page positions are kept as a running total, so finding the viewport in a
700-page book is a binary search rather than a walk — scrolling one costs the
same as scrolling a pamphlet.

The render threads take a *wish list* that the view replaces on every scroll
instead of a queue it appends to. Dragging the scrollbar across a book
therefore abandons the pages it swept over rather than rendering them minutes
later, and the page you stop on is drawn immediately. One core is left free
for the UI; the rest fill the pages around you.

Textures are dropped both by distance from the viewport and by a total memory
budget. Past ~24 megapixels a page stops being one texture and becomes a grid
of 1024×1024 tiles: only the tiles over the viewport are rasterized, and a
cheap whole-page render sits underneath so panning and zooming never expose
blank paper while the sharp tiles arrive. Zooming to 800% therefore costs
about what a screenful costs, not what a page would.

That last part needs `hayro` to rasterize an arbitrary sub-rectangle of a
page, which stock hayro cannot do, so `vendor/hayro` carries a small additive
patch adding an offset to `RenderSettings`. Only the thin facade crate is
vendored — the interpreter, the parser and the codecs still come from
crates.io. See `vendor/hayro/RAVEN-PATCH.md`.

## Roadmap

- Highlight search matches on the page
- Thumbnails in the sidebar
- Filling forms
- Drawing freehand (ink) annotations
- Search results highlighted on the page (the text layer makes this cheap now)
- Signing
- Password-protected PDFs (the engine supports them; the prompt is missing)
- Resizing pictures in documents; editing table cells
- Headers, footers and notes when reading and writing Word files
- Printing (the PDF export's layout would serve it)
