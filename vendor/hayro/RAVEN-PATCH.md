# Why this is here

This is [hayro](https://github.com/LaurenzV/hayro) 0.7.1, unchanged except
for one thing: `RenderSettings` gained `x_offset` and `y_offset`.

Stock hayro can only rasterize a page from its top-left corner. `width` and
`height` crop, but there is no way to say *where* to crop from, so showing a
corner of a page at 8× meant rasterizing the whole page at 8× — hundreds of
megabytes for a screenful. Raven Viewer needs to draw a page as tiles, which
needs an arbitrary sub-rectangle.

Only the thin `hayro` facade crate is vendored. `hayro-interpret`,
`hayro-syntax` and the font and image codecs — all of the actual work — still
come from crates.io.

## The whole patch

```diff
   pub height: Option<u16>,
+  /// How far to shift the contents left, in scaled pixels. …
+  pub x_offset: f32,
+  /// How far to shift the contents up, in scaled pixels.
+  pub y_offset: f32,
   pub bg_color: AlphaColor<Srgb>,

   /* Default::default */
+  x_offset: 0.0,
+  y_offset: 0.0,

-  let initial_transform = Affine::scale_non_uniform(x_scale as f64, y_scale as f64)
-      * page.initial_transform(true).to_kurbo();
+  let initial_transform = Affine::translate((
+      -(render_settings.x_offset as f64),
+      -(render_settings.y_offset as f64),
+  )) * Affine::scale_non_uniform(x_scale as f64, y_scale as f64)
+      * page.initial_transform(true).to_kurbo();
```

The clip path and the interpreter's clip rect are both already derived from
`initial_transform`, so they follow without further change.

## Re-syncing on a new hayro release

```sh
V=0.8.0
rm -rf vendor/hayro
cp -r ~/.cargo/registry/src/index.crates.io-*/hayro-$V vendor/hayro
rm -f vendor/hayro/Cargo.lock vendor/hayro/.cargo-ok vendor/hayro/Cargo.toml.orig
# re-apply the diff above, then:
cargo test rendering_a_tile_picks_out_that_part_of_the_page
```

That test renders three tiles of a page with known contents and fails if the
offset is missing or wrong, so a re-sync that drops the patch is caught rather
than silently falling back to rendering every tile as the page's top-left
corner.

## Upstreaming

This is deliberately shaped as a patch worth sending to hayro: it is additive,
defaults to the current behaviour, and costs nothing when unused. If it lands
upstream, delete this directory and the `[patch.crates-io]` block in
`../../Cargo.toml`.
