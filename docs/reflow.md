# Grid Reflow on Resize

## Problem

When the grid is resized smaller (e.g. on font zoom-in), the current
implementation (`resize_buffer` in `grid.rs`) naively copies the
top-left `min(old_rows, new_rows) × min(old_cols, new_cols)` region
and discards the rest. Content that overflows the new bounds is
gone forever — zooming out doesn't bring it back, scrollback doesn't
capture it. This causes visible data loss in common workflows:

1. User has output on screen, some lines longer than the new width.
2. User zooms in (`Cmd+=`). Grid shrinks. Long lines are truncated.
3. User zooms out (`Cmd+-`). Grid grows back. Truncated content is
   blank — the bytes were dropped during step 2.

Modern terminal emulators (ghostty, alacritty, kitty, wezterm) solve
this with **reflow**: they treat consecutive wrapped rows as one
logical line and re-lay out logical lines at the new width. Nothing
is lost because logical content is preserved independent of physical
layout.

## Design

### Per-row wrap flag

Each physical row gets a boolean `wrapped` flag that means
*"this row's content continues on the next physical row; they belong
to the same logical line"*. The flag is set when `put_char` auto-wraps
past the right margin (`col >= cols`). It is NOT set when a line
ends because of an explicit `\n` or because the user left trailing
blanks.

Storage choice: parallel `Vec<bool>` of length `size.rows`. Keeping
it separate from `Vec<Cell>` avoids widening the `Cell` struct and
makes the reflow pass simpler. Scrollback rows each carry their own
flag as well — this requires changing the scrollback element type
from `Vec<Cell>` to a small struct `{ cells: Vec<Cell>, wrapped: bool }`.

### Logical lines

A logical line is a sequence of cells that spans one or more
physical rows connected by wrap flags. Collection rule:

```
logical_lines = []
current = []
for each row r:
    append r's cells (trimmed of trailing blanks on non-wrapped rows)
      to current
    if r.wrapped == false:
        push current into logical_lines; reset current
```

Scrollback rows are processed first, then primary rows. The cursor's
current physical row is just another row in this walk — but its
position within the current logical line must be remembered so we
can restore it after reflow.

### Laying out at the new width

Given a sequence of logical lines and target `new_cols`, build a
new sequence of physical rows:

```
new_rows = []
for each logical line L:
    if L is empty:
        push empty row with wrapped = false
        continue
    chunk L into pieces of length new_cols:
        all but the last piece → wrapped = true
        last piece → wrapped = false, right-pad with blanks
    push all pieces
```

After layout, split the resulting physical row list into two parts:

- The **last `new_rows` rows** become the visible grid.
- Everything before that goes into scrollback (newest-first into
  `VecDeque::push_back`), honoring `scrollback_limit`.

If there are fewer than `new_rows` rows total, pad the top with
blanks (or, equivalently, keep all rows and fill the missing top
rows with blank non-wrapped rows).

### Cursor repositioning

The cursor's (row, col) in the old grid maps to a position within a
logical line. After reflow, we re-scan the new physical rows to
find which row and column that same logical position lands on. In
practice:

1. Before reflow, compute `cursor_abs = cumulative cell count from
   the start of the cursor's logical line`.
2. After reflow, walk the new rows belonging to the same logical
   line and locate `cursor_abs`.
3. If the cursor was in trailing blanks that got trimmed, clamp to
   the nearest valid position.

### Alt screen

The alt screen is used by fullscreen apps (vim, less, htop) that
already know how to redraw on resize — it's not scrollable and
doesn't accumulate history. Reflowing it would be incorrect. On
resize while in alt screen, we simply truncate/pad the alt cells
with the existing `resize_buffer` approach and let the app's
SIGWINCH handler redraw.

We still reflow the `primary_snapshot` underneath, so that on
`\e[?1049l` (exit alt screen) the user's shell history is intact.

### Scroll region

The scroll region (DECSTBM) is reset to the full screen on resize,
matching xterm behavior. Apps re-issue DECSTBM after SIGWINCH.

### Explicit newlines vs wrap

An explicit `\n` (or CR+LF from the shell) advances to the next row
without setting the wrap flag. This is how we distinguish "logical
paragraph break" from "ran out of screen width". Long terminal
output that predates a newline is one logical line; each shell
command's output is typically many logical lines.

## Implementation Plan

### Step 1 — data model

- [ ] Add `row_wrapped: Vec<bool>` field to `Grid`.
- [ ] Initialize and keep in sync with `size.rows` on `new()`.
- [ ] Replace `scrollback: VecDeque<Vec<Cell>>` with
      `scrollback: VecDeque<ScrollbackRow>` where `ScrollbackRow`
      holds `{ cells: Vec<Cell>, wrapped: bool }`.
- [ ] Update `row_at`, `push_scrollback`, and any scrollback
      consumers to use the new type.

### Step 2 — wrap flag tracking

- [ ] In `put_char`, when auto-wrap triggers (`col >= cols`), set
      `row_wrapped[cursor.row] = true` *before* calling `wrap()`.
- [ ] On explicit `\n`, CRLF, LF, or any path that advances rows
      without the cell buffer being full, leave the flag at `false`
      (the default).
- [ ] Any operation that inserts/deletes rows (`IL`, `DL`, scroll
      up/down) must move the wrap flags in lockstep with the cells.
- [ ] When a row is cleared (`EL`, `\r`, overwriting), the wrap
      flag for that row must reset to `false` only if the clearing
      extends past the last cell. Safer default: reset on any
      operation that explicitly ends the line (CR, LF).

### Step 3 — the reflow algorithm

Add a standalone pure function:

```rust
fn reflow(
    primary: &[Cell], primary_wrapped: &[bool],
    scrollback: &VecDeque<ScrollbackRow>,
    old_cols: usize,
    new_rows: usize,
    new_cols: usize,
    scrollback_limit: usize,
    cursor: Cursor,
) -> ReflowResult;

struct ReflowResult {
    new_cells: Vec<Cell>,          // new_rows * new_cols
    new_wrapped: Vec<bool>,        // len == new_rows
    new_scrollback: VecDeque<ScrollbackRow>,
    new_cursor: Cursor,
}
```

Steps inside `reflow`:

1. **Collect logical lines** from scrollback (oldest first), then
   primary rows. Track which logical line + offset the cursor is
   at.
2. **Lay out** logical lines into physical rows using `new_cols`.
3. **Split** physical rows into `scrollback` (all rows before the
   last `new_rows`) and `visible` (the last `new_rows` rows),
   capping scrollback to `scrollback_limit`.
4. **Flatten** visible rows into `new_cells` + `new_wrapped`.
5. **Compute new cursor** by finding the new physical row/column
   that matches the cursor's pre-reflow logical offset.

### Step 4 — wire it into `Grid::resize`

- [ ] Replace the existing `resize_buffer` call on the primary
      screen with `reflow`.
- [ ] Keep `resize_buffer` behavior for the alt screen path
      (`in_alt_screen == true`) and the `primary_snapshot`.
      - Actually, the snapshot holds the primary screen while the
        user is in the alt screen. We *should* reflow the snapshot
        too so the primary state is intact when the alt screen
        exits. Do both: reflow the snapshot's cells/wrapped using
        the same algorithm, with the snapshot's own scrollback view
        (scrollback is shared).
- [ ] Update cursor, scroll region, and wrap flags from the
      `ReflowResult`.

### Step 5 — tests

Unit tests in `purrtty-term/src/lib.rs`:

- [ ] `shrink_then_grow_preserves_content` (already written; will
      flip from FAIL → OK after reflow lands).
- [ ] `shrink_wrapped_content_reflows`: explicitly-long line should
      reflow across multiple rows at smaller width.
- [ ] `grow_unwraps_when_cols_increase`: content that was wrapped
      at old width merges back into one row at the new width.
- [ ] `explicit_newlines_not_merged`: rows ending in `\n` stay
      distinct after shrink/grow cycles.
- [ ] `scrollback_reflows_with_screen`: push content into scrollback,
      resize, scroll back, verify content intact and re-flowed.
- [ ] `cursor_survives_reflow`: cursor ends up at the right logical
      position after resize in both directions.
- [ ] `alt_screen_resize_is_not_reflowed`: entering alt screen,
      resizing, and exiting restores the primary as it was (with the
      primary reflowed correctly).

### Step 6 — UI integration

No changes needed in `purrtty-ui` or `purrtty-app`: the resize is
still triggered by `Grid::resize`, and the renderer reads cells via
`grid.cell(r, c)` / `grid.row_at(...)` as before. The new
`row_wrapped` flag is internal to the grid.

## Risks / edge cases

- **Wide characters** split across wrap: a CJK glyph straddling the
  right margin already wraps in `put_char`. Reflow must not split a
  wide glyph's two cells across rows — treat the pair as an atomic
  unit during layout.
- **Scrollback capacity**: reflowing at a much narrower width
  multiplies physical row count. Make sure we enforce
  `scrollback_limit` by dropping oldest rows, not by refusing to
  push.
- **Memory cost**: reflow walks the entire primary + scrollback
  cell buffer on each resize. With a 10k-row scrollback that's
  ~10k × 80 cells = ~800k cells per reflow. Acceptable for an
  interactive resize event (fires only on zoom / window resize).
- **Performance**: `Vec<Cell>` copies dominate. Pre-allocate with
  capacity hints. Still O(N) in total cells, which is fine.
- **IL / DL / scroll within a region**: when a line is inserted or
  deleted, we must also insert/delete its wrap flag slot, and the
  logical-line boundary around it is invalidated. Safest rule: any
  line modification resets the wrap flag on the affected rows to
  `false` and on the row above as well (so reflow treats the split
  as a hard break).

## Out of scope (for this pass)

- Rewrap animation / incremental reflow during an active drag
  resize. We reflow once per `resize()` call; if the user is mid-drag
  that's fine.
- Ligature / grapheme cluster preservation across wraps. We already
  don't do ligatures; graphemes are handled cell-by-cell so the
  reflow just moves cells around.
- Preserving the exact column offset of the cursor when it was
  sitting in trailing blanks. We clamp to the last non-blank cell
  of the logical line in that case.
