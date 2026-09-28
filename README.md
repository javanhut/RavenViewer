# Raven Viewer

A fast, free document reader for Raven Linux. Open a PDF or a DOCX and read
it, mark it up, edit it, or combine several into one — no browser, no
account, nothing to pay for.

## What it does

| | |
|---|---|
| **PDF** | Continuous pages, rendered in pure Rust ([hayro](https://github.com/LaurenzV/hayro)) on background threads; only pages near the viewport are rasterized, so a 700-page book opens in ~0.2s |
| **Jumping** | Fling the scrollbar or jump to a chapter and the page you land on is drawn first — work for pages you scrolled past is dropped, not queued |
| **Go to page** | Click the page counter or `Ctrl+G`; `Ctrl+Home` / `Ctrl+End` for the ends |
| **Contents** | The document outline in the sidebar; click a chapter to jump |
| **Notes** | Annotations (comments, highlights, stamps…) listed with their page; click to jump to the spot |
| **Search** | `Ctrl+F`, Enter for next, Shift+Enter for previous. Pages are searched lazily from where you are and stop at the first hit; tolerant of lost word spaces and ligatures |
| **Zoom** | Fit width by default; `Ctrl` + scroll, pinch, `Ctrl++` / `Ctrl+−`, `Ctrl+0` to re-fit. Past the point where a whole page is a sensible thing to draw, pages are tiled and only the tiles on screen are rasterized, so 800% costs a screenful rather than a page |
| **Dark pages** | Menu → Dark Pages, for reading at night |
| **Resume** | Reopening a file returns to the page you were on |
| **Select & copy** | Drag across PDF text, double-click a word, triple-click a line, Shift+click to extend; `Ctrl+C` copies. Works on OCR'd scans too, and on pages turned sideways |
| **Mark up PDFs** | Highlight (four colours), underline or strike out the selection; add sticky notes and text boxes; edit or delete them from the page's right-click menu or the Notes sidebar. Written as standard annotations with appearance streams, so every reader shows them |
| **Pages** | Right-click a page (or Menu → Page) to rotate it, move it up or down, or delete it |
| **Undo & save** | `Ctrl+Z` / `Ctrl+Shift+Z` step through PDF edits; `Ctrl+S` saves, `Ctrl+Shift+S` saves a copy. Closing or opening another file with unsaved changes asks first |
| **Edit DOCX** | `Ctrl+E` or the pencil: type into the document, bold / italic / underline / highlight, and headings, quotes and bullets from the formatting bar. Paragraphs you don't touch are saved byte for byte, so images, fields and styles survive; paragraphs holding images, equations or fields are shown read-only rather than risk them |
| **Combine** | Menu → Combine Files…: pick PDFs (or DOCX files), order them, save. PDFs keep every file's bookmarks under an entry per file; DOCX files follow one another on new pages, bringing their images, lists and styles |
| **DOCX** | A clean reading view: headings, emphasis, lists, tables, quotes |
| **Look** | GTK 4 + libadwaita with Raven Glass; theme, accent and transparency follow `~/.config/raven/desktop.toml` |

## Use

```sh
raven-viewer book.pdf            # open a file
raven-viewer open notes.docx     # same, `open` is optional
raven-viewer                     # empty window: Open… or drag a file in
raven-viewer combine out.pdf a.pdf b.pdf   # join files without opening a window
raven-viewer set-default         # make it the default for PDF and DOCX
```

A second `raven-viewer FILE` hands the file to the running instance, so
opening from the file manager or a browser download is instant.

### Keyboard

| Action | Keys |
|---|---|
| Open | `Ctrl+O` |
| Save / Save As | `Ctrl+S` / `Ctrl+Shift+S` |
| Find | `Ctrl+F` |
| Copy selection | `Ctrl+C` |
| Select the page's text | `Ctrl+A` |
| Highlight selection | `Ctrl+H` |
| Undo / redo | `Ctrl+Z` / `Ctrl+Shift+Z` |
| Edit DOCX | `Ctrl+E`, then `Ctrl+B` / `Ctrl+I` / `Ctrl+U` |
| Zoom in / out / fit | `Ctrl++` / `Ctrl+−` / `Ctrl+0` |
| Next / previous page | `N` / `P`, `Ctrl+PgDn` / `Ctrl+PgUp` |
| Go to page | `Ctrl+G` |
| First / last page | `Ctrl+Home` / `Ctrl+End` |
| Sidebar | `F9` |
| Shortcuts | `Ctrl+?` |
| Close | `Ctrl+W` |

## Build

```sh
make            # release build
make run FILE=book.pdf
sudo make install && raven-viewer set-default
```

or with ImLazy: `imlazy build`, `imlazy dev -- book.pdf`, `imlazy install`.

No C toolchain beyond what GTK already needs, no libclang. The first build
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
# the selectable text of a page, as it would be copied:
RAVEN_TEST_PDF=book.pdf RAVEN_TEST_PAGE=30 cargo test text_layer -- --ignored --nocapture
# drive the real window offscreen (open, select, copy, highlight, rotate,
# undo, save; then edit and save a DOCX), a PNG per step:
gtk4-broadwayd :7 & GDK_BACKEND=broadway BROADWAY_DISPLAY=:7 RAVEN_DRIVE_DIR=/tmp/shots \
  RAVEN_DRIVE_PDF=a.pdf RAVEN_DRIVE_DOCX=b.docx cargo test drive_the_window -- --ignored --nocapture
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
