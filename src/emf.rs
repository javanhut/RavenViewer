//! Windows Enhanced Metafiles (EMF): the vector pictures Office keeps logos,
//! charts and diagrams in. Nothing on Linux reads them, so they are drawn
//! here, record by record, with Cairo — onto a PDF page as vectors, or into
//! pixels for the editor.
//!
//! What is drawn: lines, polygons, Béziers and paths (filled, stroked,
//! clipped to), rectangles and ellipses, pens and brushes, text, and the
//! bitmaps a metafile carries (StretchDIBits, BitBlt, StretchBlt,
//! AlphaBlend). The mapping modes and world transforms place it all.
//! EMF+ records (inside comments) are skipped: Office writes the same
//! drawing as plain EMF records alongside them.

const HEADER: u32 = 1;

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    u32_at(b, at) as i32
}

fn i16_at(b: &[u8], at: usize) -> i16 {
    b.get(at..at + 2).map_or(0, |s| i16::from_le_bytes([s[0], s[1]]))
}

fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(b, at))
}

pub fn is_emf(data: &[u8]) -> bool {
    u32_at(data, 0) == HEADER && data.get(40..44) == Some(b" EMF")
}

/// The picture's size, in points, from its frame.
pub fn size(data: &[u8]) -> Option<(f64, f64)> {
    if !is_emf(data) {
        return None;
    }
    // The frame is in hundredths of a millimetre.
    let w = (i32_at(data, 32) - i32_at(data, 24)) as f64 / 100.0 * 72.0 / 25.4;
    let h = (i32_at(data, 36) - i32_at(data, 28)) as f64 / 100.0 * 72.0 / 25.4;
    (w > 0.0 && h > 0.0).then_some((w, h))
}

#[derive(Clone, Copy)]
struct Pen {
    color: (f64, f64, f64),
    width: f64,
    none: bool,
}

#[derive(Clone, Copy)]
struct Brush {
    color: (f64, f64, f64),
    none: bool,
}

#[derive(Clone)]
struct Font {
    height: f64,
    weight: i32,
    italic: bool,
    face: String,
}

#[derive(Clone)]
enum Object {
    Pen(Pen),
    Brush(Brush),
    Font(Font),
}

#[derive(Clone)]
struct State {
    map_mode: u32,
    window_org: (f64, f64),
    window_ext: (f64, f64),
    viewport_org: (f64, f64),
    viewport_ext: (f64, f64),
    world: cairo::Matrix,
    pen: Pen,
    brush: Brush,
    font: Font,
    text_color: (f64, f64, f64),
    text_align: u32,
    winding: bool,
    at: (f64, f64),
}

fn colorref(c: u32) -> (f64, f64, f64) {
    ((c & 0xFF) as f64 / 255.0, ((c >> 8) & 0xFF) as f64 / 255.0, ((c >> 16) & 0xFF) as f64 / 255.0)
}

struct Player<'a> {
    cr: &'a cairo::Context,
    state: State,
    saved: Vec<State>,
    objects: Vec<Option<Object>>,
    /// Device pixels to the target rectangle.
    target: cairo::Matrix,
    /// Device pixels per millimetre, for the metric mapping modes.
    px_per_mm: (f64, f64),
    in_path: bool,
}

/// Draw the metafile into `x, y, w, h` (user space) on `cr`. Returns false
/// when it is not one that can be read.
pub fn draw(data: &[u8], cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64) -> bool {
    if !is_emf(data) || w <= 0.0 || h <= 0.0 {
        return false;
    }
    let (dev_w, dev_h) = (i32_at(data, 72).max(1) as f64, i32_at(data, 76).max(1) as f64);
    let (mm_w, mm_h) = (i32_at(data, 80).max(1) as f64, i32_at(data, 84).max(1) as f64);
    let px_per_mm = (dev_w / mm_w, dev_h / mm_h);
    // The frame (hundredths of a millimetre) in device pixels is what fills
    // the target.
    let fl = i32_at(data, 24) as f64 / 100.0 * px_per_mm.0;
    let ft = i32_at(data, 28) as f64 / 100.0 * px_per_mm.1;
    let fr = i32_at(data, 32) as f64 / 100.0 * px_per_mm.0;
    let fb = i32_at(data, 36) as f64 / 100.0 * px_per_mm.1;
    let (fw, fh) = ((fr - fl).max(1.0), (fb - ft).max(1.0));
    let (sx, sy) = (w / fw, h / fh);
    let target = cairo::Matrix::new(sx, 0.0, 0.0, sy, x - fl * sx, y - ft * sy);

    let base = cr.matrix();
    let target = cairo::Matrix::multiply(&target, &base);
    let black = (0.0, 0.0, 0.0);
    let mut player = Player {
        cr,
        state: State {
            map_mode: 1,
            window_org: (0.0, 0.0),
            window_ext: (1.0, 1.0),
            viewport_org: (0.0, 0.0),
            viewport_ext: (1.0, 1.0),
            world: cairo::Matrix::identity(),
            pen: Pen { color: black, width: 0.0, none: false },
            brush: Brush { color: (1.0, 1.0, 1.0), none: false },
            font: Font { height: 12.0, weight: 400, italic: false, face: "sans-serif".into() },
            text_color: black,
            text_align: 0,
            winding: false,
            at: (0.0, 0.0),
        },
        saved: Vec::new(),
        objects: Vec::new(),
        target,
        px_per_mm,
        in_path: false,
    };
    cr.save().ok();
    cr.rectangle(x, y, w, h);
    cr.clip();
    let mut at = 0usize;
    let mut guard = 0;
    while at + 8 <= data.len() && guard < 2_000_000 {
        let kind = u32_at(data, at);
        let len = u32_at(data, at + 4) as usize;
        if len < 8 || at + len > data.len() {
            break;
        }
        if kind == 14 {
            break;
        }
        player.record(kind, &data[at..at + len]);
        at += len;
        guard += 1;
    }
    cr.restore().ok();
    cr.set_matrix(base);
    true
}

/// The metafile drawn into a `width` × `height` pixel image.
pub fn rasterize(data: &[u8], width: i32, height: i32) -> Option<cairo::ImageSurface> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width.max(1), height.max(1)).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        if !draw(data, &cr, 0.0, 0.0, width as f64, height as f64) {
            return None;
        }
    }
    surface.flush();
    Some(surface)
}

impl Player<'_> {
    /// Logical units to device pixels, as the mapping mode says.
    fn viewport(&self) -> cairo::Matrix {
        let s = &self.state;
        let metric = |mm: f64, flip: bool| {
            let (kx, ky) = (mm * self.px_per_mm.0, mm * self.px_per_mm.1 * if flip { -1.0 } else { 1.0 });
            cairo::Matrix::new(kx, 0.0, 0.0, ky, s.viewport_org.0 - s.window_org.0 * kx, s.viewport_org.1 - s.window_org.1 * ky)
        };
        match s.map_mode {
            // Anisotropic and isotropic: window to viewport.
            7 | 8 => {
                let kx = if s.window_ext.0 != 0.0 { s.viewport_ext.0 / s.window_ext.0 } else { 1.0 };
                let mut ky = if s.window_ext.1 != 0.0 { s.viewport_ext.1 / s.window_ext.1 } else { 1.0 };
                if s.map_mode == 7 {
                    ky = ky.signum() * kx.abs();
                }
                cairo::Matrix::new(kx, 0.0, 0.0, ky, s.viewport_org.0 - s.window_org.0 * kx, s.viewport_org.1 - s.window_org.1 * ky)
            }
            2 => metric(0.1, true),
            3 => metric(0.01, true),
            4 => metric(0.254, true),
            5 => metric(0.0254, true),
            6 => metric(25.4 / 1440.0, true),
            // Text mode: logical units are device pixels.
            _ => cairo::Matrix::new(1.0, 0.0, 0.0, 1.0, s.viewport_org.0 - s.window_org.0, s.viewport_org.1 - s.window_org.1),
        }
    }

    fn apply(&self) {
        let m = cairo::Matrix::multiply(&cairo::Matrix::multiply(&self.state.world, &self.viewport()), &self.target);
        self.cr.set_matrix(m);
    }

    fn fill_and_stroke(&self, fill: bool, stroke: bool) {
        let cr = self.cr;
        cr.set_fill_rule(if self.state.winding { cairo::FillRule::Winding } else { cairo::FillRule::EvenOdd });
        if fill && !self.state.brush.none {
            let (r, g, b) = self.state.brush.color;
            cr.set_source_rgb(r, g, b);
            if stroke { cr.fill_preserve().ok() } else { cr.fill().ok() };
        }
        if stroke && !self.state.pen.none {
            let (r, g, b) = self.state.pen.color;
            cr.set_source_rgb(r, g, b);
            // A zero-width pen is one device pixel wide.
            let width = if self.state.pen.width > 0.0 { self.state.pen.width } else { cr.device_to_user_distance(1.0, 0.0).map_or(1.0, |d| d.0.abs().max(1e-6)) };
            cr.set_line_width(width);
            cr.stroke().ok();
        }
        cr.new_path();
    }

    /// Points of a poly record: 32-bit (`POINTL`) or 16-bit (`POINTS`).
    fn points(rec: &[u8], at: usize, n: usize, short: bool) -> Vec<(f64, f64)> {
        (0..n)
            .map(|i| {
                if short {
                    (i16_at(rec, at + 4 * i) as f64, i16_at(rec, at + 4 * i + 2) as f64)
                } else {
                    (i32_at(rec, at + 8 * i) as f64, i32_at(rec, at + 8 * i + 4) as f64)
                }
            })
            .collect()
    }

    fn poly(&mut self, kind: u32, rec: &[u8]) {
        let short = kind >= 85;
        let base = if short { kind - 83 } else { kind };
        let n = u32_at(rec, 24) as usize;
        let pts = Self::points(rec, 28, n.min(rec.len() / 4), short);
        if pts.is_empty() {
            return;
        }
        self.apply();
        let cr = self.cr;
        match base {
            // Polygon and polyline: from their first point.
            3 | 4 => {
                cr.move_to(pts[0].0, pts[0].1);
                for p in &pts[1..] {
                    cr.line_to(p.0, p.1);
                }
                if base == 3 {
                    cr.close_path();
                }
                if !self.in_path {
                    self.fill_and_stroke(base == 3, true);
                }
            }
            // Bézier: from its first point.
            2 => {
                cr.move_to(pts[0].0, pts[0].1);
                for c in pts[1..].chunks(3) {
                    if let [a, b, d] = c {
                        cr.curve_to(a.0, a.1, b.0, b.1, d.0, d.1);
                    }
                }
                if !self.in_path {
                    self.fill_and_stroke(false, true);
                }
            }
            // …To: from the current position.
            5 | 6 => {
                if !self.in_path || !cr.has_current_point().unwrap_or(false) {
                    cr.move_to(self.state.at.0, self.state.at.1);
                }
                if base == 6 {
                    for p in &pts {
                        cr.line_to(p.0, p.1);
                    }
                } else {
                    for c in pts.chunks(3) {
                        if let [a, b, d] = c {
                            cr.curve_to(a.0, a.1, b.0, b.1, d.0, d.1);
                        }
                    }
                }
                if let Some(last) = pts.last() {
                    self.state.at = *last;
                }
                if !self.in_path {
                    self.fill_and_stroke(false, true);
                }
            }
            _ => {}
        }
    }

    fn poly_poly(&mut self, kind: u32, rec: &[u8]) {
        let short = kind >= 85;
        let polygon = kind == 8 || kind == 91;
        let groups = u32_at(rec, 24) as usize;
        let total = u32_at(rec, 28) as usize;
        if groups > rec.len() / 4 || total > rec.len() / 4 {
            return;
        }
        let counts: Vec<usize> = (0..groups).map(|i| u32_at(rec, 32 + 4 * i) as usize).collect();
        let pts = Self::points(rec, 32 + 4 * groups, total, short);
        self.apply();
        let mut i = 0;
        for c in counts {
            let Some(group) = pts.get(i..i + c) else { break };
            i += c;
            if let Some(first) = group.first() {
                self.cr.move_to(first.0, first.1);
                for p in &group[1..] {
                    self.cr.line_to(p.0, p.1);
                }
                if polygon {
                    self.cr.close_path();
                }
            }
        }
        if !self.in_path {
            self.fill_and_stroke(polygon, true);
        }
    }

    fn rect_of(rec: &[u8], at: usize) -> (f64, f64, f64, f64) {
        let (l, t, r, b) = (i32_at(rec, at) as f64, i32_at(rec, at + 4) as f64, i32_at(rec, at + 8) as f64, i32_at(rec, at + 12) as f64);
        (l, t, r - l, b - t)
    }

    fn record(&mut self, kind: u32, rec: &[u8]) {
        let cr = self.cr;
        match kind {
            2..=6 | 85..=89 => self.poly(kind, rec),
            7 | 8 | 90 | 91 => self.poly_poly(kind, rec),
            9 => self.state.window_ext = (i32_at(rec, 8) as f64, i32_at(rec, 12) as f64),
            10 => self.state.window_org = (i32_at(rec, 8) as f64, i32_at(rec, 12) as f64),
            11 => self.state.viewport_ext = (i32_at(rec, 8) as f64, i32_at(rec, 12) as f64),
            12 => self.state.viewport_org = (i32_at(rec, 8) as f64, i32_at(rec, 12) as f64),
            17 => self.state.map_mode = u32_at(rec, 8),
            19 => self.state.winding = u32_at(rec, 8) == 2,
            22 => self.state.text_align = u32_at(rec, 8),
            24 => self.state.text_color = colorref(u32_at(rec, 8)),
            27 => {
                self.state.at = (i32_at(rec, 8) as f64, i32_at(rec, 12) as f64);
                if self.in_path {
                    self.apply();
                    cr.move_to(self.state.at.0, self.state.at.1);
                }
            }
            54 => {
                let to = (i32_at(rec, 8) as f64, i32_at(rec, 12) as f64);
                self.apply();
                if !self.in_path || !cr.has_current_point().unwrap_or(false) {
                    cr.move_to(self.state.at.0, self.state.at.1);
                }
                cr.line_to(to.0, to.1);
                self.state.at = to;
                if !self.in_path {
                    self.fill_and_stroke(false, true);
                }
            }
            30 => {
                let (x, y, w, h) = Self::rect_of(rec, 8);
                self.apply();
                cr.new_path();
                cr.rectangle(x, y, w, h);
                cr.clip();
            }
            33 => {
                cr.save().ok();
                self.saved.push(self.state.clone());
            }
            34 => {
                let n = i32_at(rec, 8);
                let pops = if n < 0 { (-n) as usize } else { 1 };
                for _ in 0..pops {
                    if let Some(s) = self.saved.pop() {
                        self.state = s;
                        cr.restore().ok();
                    }
                }
            }
            35 => self.state.world = xform(rec, 8),
            36 => {
                let m = xform(rec, 8);
                self.state.world = match u32_at(rec, 32) {
                    1 => cairo::Matrix::identity(),
                    2 => cairo::Matrix::multiply(&m, &self.state.world),
                    3 => cairo::Matrix::multiply(&self.state.world, &m),
                    _ => m,
                };
            }
            37 => {
                let i = u32_at(rec, 8);
                if i & 0x8000_0000 != 0 {
                    self.stock(i & 0x7FFF_FFFF);
                } else if let Some(Some(obj)) = self.objects.get(i as usize) {
                    match obj.clone() {
                        Object::Pen(p) => self.state.pen = p,
                        Object::Brush(b) => self.state.brush = b,
                        Object::Font(f) => self.state.font = f,
                    }
                }
            }
            38 => {
                let style = u32_at(rec, 12);
                let width = i32_at(rec, 16) as f64;
                let pen = Pen { color: colorref(u32_at(rec, 24)), width, none: style & 0xF == 5 };
                self.store(u32_at(rec, 8), Object::Pen(pen));
            }
            95 => {
                let style = u32_at(rec, 24);
                let pen = Pen { color: colorref(u32_at(rec, 36)), width: u32_at(rec, 28) as f64, none: style & 0xF == 5 };
                self.store(u32_at(rec, 8), Object::Pen(pen));
            }
            39 => {
                let style = u32_at(rec, 12);
                let brush = Brush { color: colorref(u32_at(rec, 16)), none: style == 1 };
                self.store(u32_at(rec, 8), Object::Brush(brush));
            }
            93 | 94 => self.store(u32_at(rec, 8), Object::Brush(Brush { color: (0.5, 0.5, 0.5), none: false })),
            82 => {
                let face: Vec<u16> = (0..32).map(|i| i16_at(rec, 40 + 2 * i) as u16).take_while(|&c| c != 0).collect();
                let font = Font {
                    height: i32_at(rec, 12) as f64,
                    weight: i32_at(rec, 28),
                    italic: rec.get(32).is_some_and(|&b| b != 0),
                    face: String::from_utf16_lossy(&face),
                };
                self.store(u32_at(rec, 8), Object::Font(font));
            }
            40 => {
                let i = u32_at(rec, 8) as usize;
                if let Some(slot) = self.objects.get_mut(i) {
                    *slot = None;
                }
            }
            42..=44 => {
                let (x, y, w, h) = Self::rect_of(rec, 8);
                self.apply();
                cr.new_path();
                if kind == 42 {
                    cr.save().ok();
                    cr.translate(x + w / 2.0, y + h / 2.0);
                    cr.scale((w / 2.0).abs().max(1e-6), (h / 2.0).abs().max(1e-6));
                    cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
                    cr.restore().ok();
                } else {
                    cr.rectangle(x, y, w, h);
                }
                if !self.in_path {
                    self.fill_and_stroke(true, true);
                }
            }
            59 => {
                self.in_path = true;
                cr.new_path();
            }
            60 => self.in_path = false,
            61 => cr.close_path(),
            62 => self.fill_and_stroke(true, false),
            63 => self.fill_and_stroke(true, true),
            64 => self.fill_and_stroke(false, true),
            67 => {
                cr.set_fill_rule(if self.state.winding { cairo::FillRule::Winding } else { cairo::FillRule::EvenOdd });
                cr.clip();
            }
            68 => {
                self.in_path = false;
                cr.new_path();
            }
            75 => {
                // Back to the whole picture: with no region given, the clip
                // is reset.
                if u32_at(rec, 8) == 0 {
                    cr.reset_clip();
                }
            }
            76 | 77 | 81 | 114 => self.bitmap(kind, rec),
            84 => self.text(rec),
            _ => {}
        }
    }

    fn store(&mut self, i: u32, obj: Object) {
        let i = i as usize;
        if i > 1 << 16 {
            return;
        }
        if self.objects.len() <= i {
            self.objects.resize(i + 1, None);
        }
        self.objects[i] = Some(obj);
    }

    fn stock(&mut self, i: u32) {
        let gray = |v: f64| (v, v, v);
        match i {
            0 => self.state.brush = Brush { color: gray(1.0), none: false },
            1 => self.state.brush = Brush { color: gray(0.75), none: false },
            2 => self.state.brush = Brush { color: gray(0.5), none: false },
            3 => self.state.brush = Brush { color: gray(0.25), none: false },
            4 => self.state.brush = Brush { color: gray(0.0), none: false },
            5 => self.state.brush.none = true,
            6 => self.state.pen = Pen { color: gray(1.0), width: 0.0, none: false },
            7 => self.state.pen = Pen { color: gray(0.0), width: 0.0, none: false },
            8 => self.state.pen.none = true,
            _ => {}
        }
    }

    fn bitmap(&mut self, kind: u32, rec: &[u8]) {
        // Where it goes, and where its bitmap header and pixels are.
        let (dest, bmi_at, bits_at) = match kind {
            81 => {
                let (x, y) = (i32_at(rec, 24) as f64, i32_at(rec, 28) as f64);
                let (cx, cy) = (i32_at(rec, 72) as f64, i32_at(rec, 76) as f64);
                ((x, y, cx, cy), (u32_at(rec, 48), u32_at(rec, 52)), (u32_at(rec, 56), u32_at(rec, 60)))
            }
            76 | 77 | 114 => {
                let dest = (i32_at(rec, 24) as f64, i32_at(rec, 28) as f64, i32_at(rec, 32) as f64, i32_at(rec, 36) as f64);
                let base = 24 + 16 + 4 + 8 + 24 + 4 + 4;
                (dest, (u32_at(rec, base), u32_at(rec, base + 4)), (u32_at(rec, base + 8), u32_at(rec, base + 12)))
            }
            _ => return,
        };
        let (x, y, w, h) = dest;
        self.apply();
        if bmi_at.1 == 0 {
            // No bitmap: the brush fills the rectangle.
            if !self.state.brush.none && kind != 114 {
                let (r, g, b) = self.state.brush.color;
                self.cr.set_source_rgb(r, g, b);
                self.cr.rectangle(x, y, w, h);
                self.cr.fill().ok();
            }
            return;
        }
        let bmi = rec.get(bmi_at.0 as usize..(bmi_at.0 + bmi_at.1) as usize).unwrap_or_default();
        let bits = rec.get(bits_at.0 as usize..(bits_at.0 as usize + bits_at.1 as usize).min(rec.len())).unwrap_or_default();
        let Some(image) = dib(bmi, bits, kind == 114) else { return };
        let (iw, ih) = (image.width() as f64, image.height() as f64);
        let cr = self.cr;
        cr.save().ok();
        cr.translate(x, y);
        cr.scale(w / iw, h / ih);
        cr.set_source_surface(&image, 0.0, 0.0).ok();
        cr.source().set_filter(cairo::Filter::Good);
        cr.rectangle(0.0, 0.0, iw, ih);
        cr.fill().ok();
        cr.restore().ok();
    }

    fn text(&mut self, rec: &[u8]) {
        let (rx, ry) = (i32_at(rec, 36) as f64, i32_at(rec, 40) as f64);
        let n = u32_at(rec, 44) as usize;
        let off = u32_at(rec, 48) as usize;
        let units: Vec<u16> = (0..n).map(|i| i16_at(rec, off + 2 * i) as u16).collect();
        let text = String::from_utf16_lossy(&units);
        if text.trim().is_empty() {
            return;
        }
        // Text is drawn upright whatever the mapping, at its size on the page.
        self.apply();
        let cr = self.cr;
        let (dx, dy) = cr.user_to_device(rx, ry);
        let Ok((_, hy)) = cr.user_to_device_distance(0.0, self.state.font.height.abs().max(1.0)) else { return };
        let size = hy.abs();
        cr.save().ok();
        cr.identity_matrix();
        let font = &self.state.font;
        let weight = if font.weight >= 600 { cairo::FontWeight::Bold } else { cairo::FontWeight::Normal };
        let slant = if font.italic { cairo::FontSlant::Italic } else { cairo::FontSlant::Normal };
        cr.select_font_face(if font.face.is_empty() { "sans-serif" } else { &font.face }, slant, weight);
        // A negative height is the characters' height without the leading.
        cr.set_font_size(if font.height < 0.0 { size } else { size * 0.8 });
        let extents = cr.font_extents().ok();
        let width = cr.text_extents(&text).map_or(0.0, |e| e.x_advance());
        let align = self.state.text_align;
        let x = match align & 6 {
            6 => dx - width / 2.0,
            2 => dx - width,
            _ => dx,
        };
        let y = match align & 24 {
            24 => dy,
            8 => dy - extents.map_or(0.0, |e| e.descent()),
            _ => dy + extents.map_or(size * 0.8, |e| e.ascent()),
        };
        let (r, g, b) = self.state.text_color;
        cr.set_source_rgb(r, g, b);
        cr.move_to(x, y);
        cr.show_text(&text).ok();
        cr.restore().ok();
    }
}

fn xform(rec: &[u8], at: usize) -> cairo::Matrix {
    cairo::Matrix::new(
        f32_at(rec, at) as f64,
        f32_at(rec, at + 4) as f64,
        f32_at(rec, at + 8) as f64,
        f32_at(rec, at + 12) as f64,
        f32_at(rec, at + 16) as f64,
        f32_at(rec, at + 20) as f64,
    )
}

/// A device-independent bitmap as an image: 1, 4, 8, 16, 24 or 32 bits a
/// pixel, bottom-up or top-down. `alpha` keeps a 32-bit bitmap's alpha
/// (as AlphaBlend has it, premultiplied).
fn dib(bmi: &[u8], bits: &[u8], alpha: bool) -> Option<cairo::ImageSurface> {
    let header = u32_at(bmi, 0) as usize;
    let w = i32_at(bmi, 4);
    let h = i32_at(bmi, 8);
    let count = u16::from_le_bytes([*bmi.get(14)?, *bmi.get(15)?]) as usize;
    let compression = u32_at(bmi, 16);
    if w <= 0 || h == 0 || w > 1 << 14 || h.abs() > 1 << 14 || !matches!(count, 1 | 4 | 8 | 16 | 24 | 32) || (compression != 0 && compression != 3) {
        return None;
    }
    let (w, top_down, h) = (w as usize, h < 0, h.unsigned_abs() as usize);
    let colors = match u32_at(bmi, 32) as usize {
        0 if count <= 8 => 1 << count,
        n => n,
    };
    let palette_at = header + if compression == 3 { 12 } else { 0 };
    let palette: Vec<(u8, u8, u8)> = (0..colors.min(256))
        .map(|i| {
            let p = palette_at + 4 * i;
            (*bmi.get(p + 2).unwrap_or(&0), *bmi.get(p + 1).unwrap_or(&0), *bmi.get(p).unwrap_or(&0))
        })
        .collect();
    let stride_in = (w * count).div_ceil(32) * 4;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).ok()?;
    let stride = surface.stride() as usize;
    {
        let mut out = surface.data().ok()?;
        for row in 0..h {
            let src_row = if top_down { row } else { h - 1 - row };
            let line = bits.get(src_row * stride_in..src_row * stride_in + stride_in);
            let Some(line) = line else { continue };
            for col in 0..w {
                let (r, g, b, a) = match count {
                    32 => {
                        let p = &line[col * 4..col * 4 + 4];
                        (p[2], p[1], p[0], if alpha { p[3] } else { 255 })
                    }
                    24 => {
                        let p = &line[col * 3..col * 3 + 3];
                        (p[2], p[1], p[0], 255)
                    }
                    16 => {
                        let v = u16::from_le_bytes([line[col * 2], line[col * 2 + 1]]);
                        let (r, g, b) = if compression == 3 { ((v >> 11) & 31, (v >> 5) & 63, v & 31) } else { ((v >> 10) & 31, (v >> 5) & 31, v & 31) };
                        let gmax = if compression == 3 { 63 } else { 31 };
                        ((r * 255 / 31) as u8, (g * 255 / gmax) as u8, (b * 255 / 31) as u8, 255)
                    }
                    _ => {
                        let per = 8 / count;
                        let byte = line[col / per];
                        let shift = 8 - count * (col % per + 1);
                        let i = ((byte >> shift) as usize) & ((1 << count) - 1);
                        let (r, g, b) = palette.get(i).copied().unwrap_or((0, 0, 0));
                        (r, g, b, 255)
                    }
                };
                // Cairo wants premultiplied alpha; AlphaBlend's already is.
                let pre = |c: u8| if alpha || a == 255 { c } else { (c as u32 * a as u32 / 255) as u8 };
                let v = (a as u32) << 24 | (pre(r) as u32) << 16 | (pre(g) as u32) << 8 | pre(b) as u32;
                out[row * stride + 4 * col..row * stride + 4 * col + 4].copy_from_slice(&v.to_ne_bytes());
            }
        }
    }
    surface.mark_dirty();
    Some(surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut r = kind.to_le_bytes().to_vec();
        r.extend_from_slice(&((8 + body.len()) as u32).to_le_bytes());
        r.extend_from_slice(body);
        r
    }

    fn le(vals: &[i32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A red square on a 100×100 picture fills the right part of the image.
    #[test]
    fn a_metafile_draws_its_shapes() {
        let mut header = le(&[0, 0, 99, 99, 0, 0, 2645, 2645]);
        header.extend_from_slice(b" EMF");
        header.extend_from_slice(&le(&[0x10000, 0, 0, 0, 0, 0, 0]));
        header.extend_from_slice(&le(&[100, 100, 26, 26]));
        let mut emf = record(1, &header);
        emf.extend(record(39, &le(&[1, 0, 0x0000FF, 0])));
        emf.extend(record(37, &le(&[1])));
        emf.extend(record(37, &le(&[0x8000_0008u32 as i32])));
        emf.extend(record(43, &le(&[50, 0, 100, 100])));
        emf.extend(record(14, &le(&[0, 0, 0])));
        assert!(is_emf(&emf));
        let image = rasterize(&emf, 100, 100).unwrap();
        let stride = image.stride() as usize;
        let mut image = image;
        let data = image.data().unwrap();
        let px = |x: usize, y: usize| u32::from_ne_bytes(data[y * stride + 4 * x..y * stride + 4 * x + 4].try_into().unwrap());
        assert_eq!(px(75, 50), 0xFFFF0000, "red where the square is");
        assert_eq!(px(25, 50) >> 24, 0, "nothing where it is not");
    }
}
