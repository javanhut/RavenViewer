//! The paintable behind each page.
//!
//! A page is not one picture but a small pile of them: a whole-page texture
//! underneath, and — once the reader zooms past the point where drawing the
//! whole page is sensible — sharp tiles over the part actually on screen.
//! Everything is placed by fractions of the page rather than pixels, so a
//! texture rendered at one zoom still lands in the right place while it is
//! being stretched to cover another.

use std::cell::{Cell, RefCell};

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene};

use crate::pdf::{RenderedTile, TileKey};

/// One texture and the fraction of the page it covers.
pub(super) struct Piece {
    key: TileKey,
    area: (f32, f32, f32, f32),
    texture: gdk::Texture,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Layers {
    /// The last whole-page texture, kept while the sharper layer is still
    /// arriving so a zoom never flashes blank paper. At most one.
    backdrop: Option<Piece>,
    /// What has been drawn for the generation in `generation`.
    current: Vec<Piece>,
    generation: u64,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PageTiles {
        pub(super) layers: RefCell<Layers>,
        pub(super) bytes: Cell<usize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PageTiles {
        const NAME: &'static str = "RavenPageTiles";
        type Type = super::PageTiles;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for PageTiles {}

    impl PaintableImpl for PageTiles {
        /// The page has no size of its own: the view sizes the widget from
        /// the PDF, and saying so here keeps a zoomed-out page from claiming
        /// the width it had at 100%.
        fn intrinsic_width(&self) -> i32 {
            0
        }

        fn intrinsic_height(&self) -> i32 {
            0
        }

        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::SIZE
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            let Some(snapshot) = snapshot.downcast_ref::<gtk::Snapshot>() else { return };
            let layers = self.layers.borrow();
            let place = |piece: &Piece| {
                let (x, y, w, h) = piece.area;
                // Tile edges are exact multiples of the tile size, so
                // neighbours share an edge to the bit and leave no seam.
                let rect = graphene::Rect::new(
                    x * width as f32,
                    y * height as f32,
                    w * width as f32,
                    h * height as f32,
                );
                snapshot.append_texture(&piece.texture, &rect);
            };
            if let Some(backdrop) = &layers.backdrop {
                place(backdrop);
            }
            for piece in &layers.current {
                place(piece);
            }
        }
    }
}

glib::wrapper! {
    pub struct PageTiles(ObjectSubclass<imp::PageTiles>) @implements gdk::Paintable;
}

impl Default for PageTiles {
    fn default() -> Self {
        Self::new()
    }
}

impl PageTiles {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Whether this piece is already drawn at `generation`.
    pub fn has(&self, key: TileKey, generation: u64) -> bool {
        let layers = self.imp().layers.borrow();
        layers.generation == generation && layers.current.iter().any(|p| p.key == key)
    }

    pub fn bytes(&self) -> usize {
        self.imp().bytes.get()
    }

    /// Take a freshly rendered piece. A piece from a newer generation retires
    /// the old layer, keeping its whole-page texture as a backdrop so the
    /// page stays covered while the sharp pieces come in.
    pub fn accept(&self, tile: RenderedTile) {
        let inner = self.imp();
        let bytes = tile.pixels.len();
        let pixels = glib::Bytes::from_owned(tile.pixels);
        let texture = gdk::MemoryTexture::new(
            tile.width as i32,
            tile.height as i32,
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            &pixels,
            tile.width as usize * 4,
        );
        let piece =
            Piece { key: tile.key, area: tile.area, texture: texture.upcast(), bytes };
        let whole = covers_page(piece.area);

        let mut layers = inner.layers.borrow_mut();
        if layers.generation != tile.generation {
            // Keep the previous whole-page texture, if there was one, rather
            // than the tiles: it covers the page on its own and is one
            // texture instead of a screenful.
            if let Some(previous) = layers.current.iter().position(|p| covers_page(p.area)) {
                layers.backdrop = Some(layers.current.swap_remove(previous));
            }
            layers.current.clear();
            layers.generation = tile.generation;
        }
        // A whole-page render at the current generation covers everything the
        // backdrop was standing in for.
        if whole {
            layers.backdrop = None;
            layers.current.retain(|p| !covers_page(p.area));
            layers.current.insert(0, piece);
        } else {
            layers.current.retain(|p| p.key != piece.key);
            layers.current.push(piece);
        }
        inner.bytes.set(
            layers.backdrop.iter().chain(layers.current.iter()).map(|p| p.bytes).sum(),
        );
        drop(layers);
        self.invalidate_contents();
    }

    /// Keep only these pieces (and the whole-page render, which is what the
    /// sharp tiles sit on). Without this, working across a zoomed page would
    /// pile up every tile it ever touched.
    pub fn retain(&self, keep: &[TileKey]) {
        let inner = self.imp();
        let mut layers = inner.layers.borrow_mut();
        let before = layers.current.len();
        layers.current.retain(|p| covers_page(p.area) || keep.contains(&p.key));
        if layers.current.len() == before {
            return;
        }
        inner.bytes.set(layers.backdrop.iter().chain(layers.current.iter()).map(|p| p.bytes).sum());
        drop(layers);
        self.invalidate_contents();
    }

    /// Drop every texture; the page goes back to blank paper.
    pub fn clear(&self) {
        let inner = self.imp();
        let mut layers = inner.layers.borrow_mut();
        if layers.backdrop.is_none() && layers.current.is_empty() {
            return;
        }
        *layers = Layers { generation: layers.generation, ..Default::default() };
        inner.bytes.set(0);
        drop(layers);
        self.invalidate_contents();
    }
}

/// Whether a piece spans the whole page, i.e. is a backing render rather than
/// a tile.
fn covers_page(area: (f32, f32, f32, f32)) -> bool {
    area.2 >= 1.0 && area.3 >= 1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::TileKey;

    fn piece(page: usize, tile: Option<(u32, u32)>, area: (f32, f32, f32, f32), edge: u32) -> RenderedTile {
        RenderedTile {
            key: TileKey { page, tile },
            generation: 1,
            width: edge,
            height: edge,
            area,
            pixels: vec![200; (edge * edge * 4) as usize],
        }
    }

    /// Where the tiles actually land, and in what order. The paintable
    /// places everything by fractions of the page, so a quarter-page tile
    /// must come out as a quarter of the widget whatever size the widget is.
    ///
    /// Both halves live in one test because GTK may only be touched from the
    /// one thread that initialized it.
    #[test]
    fn tiles_are_placed_by_their_share_of_the_page() {
        if gtk::init().is_err() {
            eprintln!("no display: skipping the paintable placement test");
            return;
        }

        let tiles = PageTiles::new();
        let quadrants = [(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)];
        for (i, &(x, y)) in quadrants.iter().enumerate() {
            tiles.accept(piece(0, Some((i as u32 % 2, i as u32 / 2)), (x, y, 0.5, 0.5), 8));
        }

        for widget in [64.0_f32, 1000.0] {
            let snapshot = gtk::Snapshot::new();
            tiles.snapshot(&snapshot, widget as f64, widget as f64);
            let node = snapshot.to_node().expect("the paintable drew nothing");
            let drawn = flatten(&node);
            assert_eq!(drawn.len(), 4, "expected one node per tile at {widget}px");
            for (i, &(x, y)) in quadrants.iter().enumerate() {
                let bounds = &drawn[i];
                let want = (x * widget, y * widget, 0.5 * widget, 0.5 * widget);
                let got = (bounds.x(), bounds.y(), bounds.width(), bounds.height());
                assert!(
                    (got.0 - want.0).abs() < 0.01
                        && (got.1 - want.1).abs() < 0.01
                        && (got.2 - want.2).abs() < 0.01
                        && (got.3 - want.3).abs() < 0.01,
                    "tile {i} at {widget}px landed at {got:?}, expected {want:?}"
                );
            }
            // Tiles must meet exactly: no seam down the middle, no overlap.
            assert_eq!(drawn[0].x() + drawn[0].width(), drawn[1].x(), "a vertical seam would show here");
            assert_eq!(drawn[0].y() + drawn[0].height(), drawn[2].y(), "a horizontal seam would show here");
        }

        // The whole-page render goes underneath, the sharp tiles over it.
        let layered = PageTiles::new();
        layered.accept(piece(0, Some((1, 1)), (0.5, 0.5, 0.5, 0.5), 8));
        layered.accept(piece(0, None, (0.0, 0.0, 1.0, 1.0), 4));
        let snapshot = gtk::Snapshot::new();
        layered.snapshot(&snapshot, 100.0, 100.0);
        let drawn = flatten(&snapshot.to_node().unwrap());
        assert_eq!(drawn.len(), 2);
        assert_eq!((drawn[0].width(), drawn[0].height()), (100.0, 100.0), "the backing is drawn first");
        assert_eq!((drawn[1].x(), drawn[1].y()), (50.0, 50.0), "the tile is drawn over it");
    }

    /// Panning must not pile up every tile the reader has passed over.
    #[test]
    fn retain_drops_tiles_that_scrolled_off_but_keeps_the_backing() {
        let tiles = PageTiles::new();
        tiles.accept(piece(0, None, (0.0, 0.0, 1.0, 1.0), 4));
        for col in 0..4 {
            tiles.accept(piece(0, Some((col, 0)), (col as f32 * 0.25, 0.0, 0.25, 1.0), 8));
        }
        let full = tiles.bytes();
        tiles.retain(&[TileKey { page: 0, tile: Some((3, 0)) }]);
        assert!(tiles.bytes() < full, "dropped tiles must give their memory back");
        assert!(tiles.has(TileKey { page: 0, tile: Some((3, 0)) }, 1), "the visible tile stays");
        assert!(!tiles.has(TileKey { page: 0, tile: Some((0, 0)) }, 1), "the tile panned off goes");
        assert!(tiles.has(TileKey { page: 0, tile: None }, 1), "the backing render always stays");
    }

    /// The bounds of every texture the paintable drew, in order.
    fn flatten(node: &gtk::gsk::RenderNode) -> Vec<gtk::graphene::Rect> {
        use gtk::gsk::RenderNodeType;
        match node.node_type() {
            RenderNodeType::ContainerNode => {
                let container = node.downcast_ref::<gtk::gsk::ContainerNode>().unwrap();
                (0..container.n_children()).flat_map(|i| flatten(&container.child(i))).collect()
            }
            _ => vec![node.bounds()],
        }
    }
}
