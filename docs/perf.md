# purrtty rendering performance — research notes

This document captures what high-performance terminal emulators do for
text rendering, what we were doing wrong, and the path forward. Written
during M4 after lag at large window sizes prompted a "are we even doing
this right?" pause.

## Standard pattern across high-perf terminals

Every fast terminal emulator uses some variation of the same recipe:

1. **Glyph atlas on the GPU.** Each unique `(font, codepoint, size,
   weight, style)` is rasterized **once** into a shared GPU texture.
   Subsequent uses just reference atlas coordinates.
2. **Shaping cache on the CPU.** Shaping (codepoint sequence → ordered
   glyph IDs + positions, including ligatures, BiDi, complex scripts)
   is expensive. Cache the result keyed by something stable like
   `(codepoint sequence + SGR attrs)`.
3. **Vertex buffer of positioned quads.** Per cell: emit one textured
   quad referencing the atlas + a color. Background colors and
   underlines are extra quads.
4. **Single (or very few) draw calls.** All quads ship to the GPU in
   one buffer; the fragment shader samples the atlas.

References:

- **Alacritty** — OpenGL, custom glyph atlas, ~500 FPS on a full screen
  ([announcement](https://jwilm.io/blog/announcing-alacritty/),
  [DeepWiki](https://deepwiki.com/alacritty/alacritty),
  [new renderer PR](https://github.com/alacritty/alacritty/pull/4373))
- **WezTerm** — HarfBuzz for shaping, GPU atlases, async fallback font
  resolution
  ([Font System](https://deepwiki.com/wezterm/wezterm/3.4-input-handling))
- **Microsoft Terminal AtlasEngine** — explicit single-quad-per-cell
  shader, atlas-only fast path
  ([AtlasEngine](https://deepwiki.com/microsoft/terminal/3.2-atlas-engine),
  [perf discussion](https://github.com/microsoft/terminal/discussions/12811))
- **Contour** — word-level shaping cache keyed by codepoints + SGR
  attrs, separate atlases for monochrome text and color emoji, single
  draw call after shaping
  ([text stack](https://contour-terminal.org/internals/text-stack/))
- **Zutty** — even more aggressive: a single OpenGL compute shader
  draws every glyph in one pass
  ([How Zutty works](https://tomscii.sig7.se/2020/11/How-Zutty-works))
- **Warp** — designed its own kerning and atlas pipeline for the
  block-grid model
  ([Warp blog](https://www.warp.dev/blog/adventures-text-rendering-kerning-glyph-atlases))

The line-level cache (hash of a whole row → shaped result) is generally
considered a worse tradeoff than glyph-level caching, because terminal
rows change frequently as data is written to stdout/stderr. Glyph-level
caching is the consensus.

## Where cosmic-text + glyphon fit

We are not writing our own glyph atlas; we're using the
[`glyphon`](https://github.com/grovesNL/glyphon) crate, which is exactly
"cosmic-text shapes on the CPU, glyphon ships glyphs to a wgpu atlas
and emits quads". So the atlas + quad pipeline is already there — the
question is purely **how to use cosmic-text efficiently**.

Two approaches surfaced in the cosmic-text [discussion #65](https://github.com/pop-os/cosmic-text/discussions/65):

- **Line-based rendering**: shape full lines into CPU textures, upload.
  Simple but redundant for repeating glyphs.
- **Glyph atlas**: cache per-glyph bitmaps in a shared GPU texture (this
  is what glyphon already does internally — every unique glyph is
  uploaded once and referenced by atlas coordinates).

Where cosmic-text rendering can still go wrong is in **how the
application drives shaping**. cosmic-text shapes lazily but you have
to use the right API.

## What cosmic-term does (and we should copy)

[cosmic-term](https://github.com/pop-os/cosmic-term) is the COSMIC
desktop terminal, built on `alacritty_terminal` and `cosmic-text` +
`glyphon`. System76 reports it matching Alacritty's performance on
vtebench and on an 8 MB text dump. It uses `glyphon` at the same major
version we use — so this is direct evidence the tools are fast enough
when driven correctly.

The pattern:

```rust
// Single Buffer for the whole terminal grid.
buffer: Arc<Buffer>

// Update one line in place.
buffer.lines[line_i].set_text(text, LineEnding::default(), attrs_list);
```

Crucially:

1. **One `Buffer`** for the whole grid, not one per row, not one per
   cell, not rebuilt every frame.
2. Per-line updates go through `buffer.lines[i].set_text(...)` which
   is the documented API for "this line changed; everything else is
   the same". cosmic-text **already does line-level dirty tracking
   internally** — only lines that were touched get re-shaped on the
   next `shape_until_scroll()` call.
3. **`AttrsList`** carried with each line update encodes per-cell
   colors, weight, italic, underline as ranges. The renderer doesn't
   need a separate "color path" — colors flow through the same shaping
   call as the text.
4. After updates, **one** `shape_until_scroll()` and **one**
   `TextRenderer::prepare()` per frame.

This is the right shape of the solution. Our cosmetic add-ons (cursor,
background colors not covered by `AttrsList`) become small wgpu quad
passes layered on top.

## Where our M3 / M4-stage-1 approaches went wrong

| Iteration | What we did | Why it was wrong |
|---|---|---|
| **M3 → M4 stage 1 (current main branch)** | Each frame, one giant `Buffer::set_text(text)` rebuilding the entire grid as one paragraph | cosmic-text re-shapes the **whole paragraph** on every `set_text`, regardless of how little changed. Soft-wrap and line breaking are computed even though we set `Wrap::None`. |
| **M4 stage 1 (per-row Buffer + content hash)** | One `Buffer` per visible row, content-hash dirty check skips unchanged rows | Line-level dirty tracking by hand on top of an API that already does it natively. Plus: each `Buffer` carries its own paragraph layout state, and we still call `set_text` per row when content changes — which is `BufferLine::set_text` × 1, not 0. |
| **M4 stage 1.5 (uncommitted: buffer pool keyed by content hash)** | Pool of pre-shaped row buffers; rotate/reuse to handle scroll | Reinventing line-level dirty tracking with a hash map. Doesn't help when rows change content (which is the actual common case the moment we add colors), and ignores cosmic-text's built-in mechanism. |

All three are different shades of "we don't trust cosmic-text and are
papering over what we think is missing". The honest version of stage 1
should have been "one `Buffer`, per-line `set_text` + per-cell
`AttrsList`, let cosmic-text do its job."

## Plan forward (revised M4 stages)

### Stage 1' — single Buffer + per-line updates (replaces current stage 1)

- Renderer owns exactly one `glyphon::Buffer`, sized to the grid.
- On each frame, walk visible rows; for each row whose content
  changed since the last frame, call `buffer.lines[i].set_text(...)`
  with an `AttrsList` populated from each cell's `Cell.fg/attrs`.
- Single `shape_until_scroll()` and single `TextRenderer::prepare()`.
- Default fg/bg from a config; per-cell fg/attrs from `AttrsList`.

This single change is expected to:

- Remove the scroll-lag complaint (line-level dirty tracking inside
  cosmic-text avoids re-shaping rows whose content didn't actually
  change in the last frame).
- Deliver per-glyph colors essentially for free (the same call carries
  them).

### Stage 2 — background colors, cursor, cell-exact positioning

- Add a wgpu quad pass for `Cell.bg` and the cursor block.
- Measure actual monospace advance from cosmic-text (`Buffer` exposes
  line metrics) instead of `CELL_WIDTH = 10.0`.
- Honor `Grid::cursor_visible()` (state already tracked from M3.5).

### Stage 3 — modifier keys + IME (unchanged from earlier plan)

### Stage 4 — only if measurements still show problems

- ASCII fast path (skip cosmic-text shaping for ASCII rows by going
  straight to a custom shaped run keyed on the cached monospace
  advance). Likely unnecessary if Stage 1' lands cleanly — cosmic-term
  hits Alacritty perf without it.
- Redraw coalescing for PTY storms (still worth doing regardless).
- Off-screen culling when `scroll_offset > 0`.

## Open questions

1. **Per-cell `AttrsList` vs. attribute-run compaction.** We could naively
   add one span per cell, or compact runs of cells with identical
   attributes into a single span. Compaction is cheap and probably
   worth doing, but worth measuring.
2. **Wide-char metric drift.** cosmic-text reports CJK advance ≈
   `1.7–2.0 ×` Latin advance for the same point size. Per-cell
   positioning gives us exact alignment but breaks ligatures inside a
   cell. We choose grid alignment over ligatures (Warp also does).
3. **Atlas growth with truecolor.** Glyphon's atlas grows with unique
   glyph shapes, not colors (color is a per-vertex attribute). So we
   pay nothing extra for SGR truecolor. Confirmed by `glyphon` source.
