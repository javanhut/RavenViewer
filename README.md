# Raven Viewer

A fast, free document reader for Raven Linux. Open a PDF or a DOCX and read
it — no browser, no account, nothing to pay for.

## What it does

| | |
|---|---|
| **PDF** | Continuous pages, rendered in pure Rust ([hayro](https://github.com/LaurenzV/hayro)) on a background thread; only pages near the viewport are rasterized, so a 700-page book opens in ~0.2s |
| **Contents** | The document outline in the sidebar; click a chapter to jump |
| **Notes** | Annotations (comments, highlights, stamps…) listed with their page; click to jump to the spot |
| **Search** | `Ctrl+F`, Enter for next, Shift+Enter for previous. Pages are searched lazily from where you are and stop at the first hit; tolerant of lost word spaces and ligatures |
| **Zoom** | Fit width by default; `Ctrl` + scroll, pinch, `Ctrl++` / `Ctrl+−`, `Ctrl+0` to re-fit |
| **Dark pages** | Menu → Dark Pages, for reading at night |
| **Resume** | Reopening a file returns to the page you were on |
| **DOCX** | A clean reading view: headings, emphasis, lists, tables, quotes |
| **Look** | GTK 4 + libadwaita with Raven Glass; theme, accent and transparency follow `~/.config/raven/desktop.toml` |

## Use

```sh
raven-viewer book.pdf            # open a file
raven-viewer open notes.docx     # same, `open` is optional
raven-viewer                     # empty window: Open… or drag a file in
raven-viewer set-default         # make it the default for PDF and DOCX
```

A second `raven-viewer FILE` hands the file to the running instance, so
opening from the file manager or a browser download is instant.

### Keyboard

| Action | Keys |
|---|---|
| Open | `Ctrl+O` |
| Find | `Ctrl+F` |
| Zoom in / out / fit | `Ctrl++` / `Ctrl+−` / `Ctrl+0` |
| Next / previous page | `N` / `P`, `Ctrl+PgDn` / `Ctrl+PgUp` |
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
```

## Tests

```sh
cargo test
# engine timings and a PNG of page 1 on any PDF:
RAVEN_TEST_PDF=book.pdf RAVEN_TEST_QUERY="word" cargo test smoke -- --ignored --nocapture
```

## Roadmap

- Highlight search matches on the page
- Thumbnails in the sidebar
- Adding annotations and highlights, filling forms
- Signing
- Password-protected PDFs (the engine supports them; the prompt is missing)
