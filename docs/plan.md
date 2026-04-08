# purrtty — Warp-Inspired Terminal Emulator for macOS (v0.1 Plan)

## Context

새로운 그린필드 프로젝트. 목표는 Warp 스타일의 모던 GPU 가속 터미널 에뮬레이터 `purrtty`를 macOS용 네이티브 GUI 앱으로 만드는 것.

사용자 결정 사항:
- **스택**: Rust + wgpu (Warp 스타일 GPU 렌더링)
- **v1 스코프**: 최소 코어 — PTY + GPU 렌더링 + 기본 VT 파서 (한 창에서 `$SHELL`이 실제로 돌고 텍스트가 제대로 그려지는 수준)
- **AI**: v1 제외, v2에서 재검토
- **블록 모델/탭/분할/팔레트**: v2 이후

v0.1은 "Alacritty급 최소 터미널을 Rust로 직접 세운다"는 목표이고, 아키텍처는 나중에 Warp 스타일 블록/에이전트가 얹힐 수 있도록 느슨하게 층을 나눠둔다.

## Reference: Warp 핵심 (나중 마일스톤용)

- GPU 렌더링 (wgpu), 자체 UI 프레임워크 (hybrid immediate/retained)
- Block 기반 데이터 모델 (command + output + metadata가 단일 유닛)
- IDE급 멀티라인 에디터, 구문 하이라이트, 자동완성
- Cmd+P 커맨드 팔레트, AI (`#` 자연어), Warp Drive 공유
- 144+ FPS, 평균 redraw 1.9ms

v0.1은 이 중 **GPU 렌더링 기반 + VT 에뮬레이션**만 잡고, 나머지는 블록 레이어를 얹을 수 있는 훅만 남겨둔다.

## Tech Stack (v0.1)

| 레이어 | 크레이트 | 이유 |
|---|---|---|
| 윈도우/이벤트 | `winit` | macOS에서 가장 잘 굴러가는 de-facto 윈도잉 |
| GPU | `wgpu` | Metal 백엔드로 macOS 네이티브, 나중 크로스플랫폼 여지 |
| 텍스트/폰트 | `glyphon` (`cosmic-text` + wgpu) | GPU 글리프 아틀라스 제공. 서브픽셀, 리가처, 이모지 지원 |
| VT 파서 | `vte` | Alacritty가 쓰는 사실상 표준. ANSI/VT100/xterm 시퀀스 |
| PTY | `portable-pty` | 크로스플랫폼 PTY 추상화 (macOS openpty 포함) |
| 비동기/채널 | `tokio` + `crossbeam-channel` | PTY I/O ↔ 렌더 스레드 분리 |
| 로깅 | `tracing` + `tracing-subscriber` | 구조화 로그 |
| 에러 | `anyhow`, `thiserror` | 관례 |
| 설정 | `serde` + `toml` | `~/.config/purrtty/config.toml` |

**대안 메모**: Zed의 `gpui` 크레이트도 후보였지만, API가 moving target이고 외부 사용 사례가 적음. `winit` + `wgpu` + `glyphon` 조합이 더 독립적이고 문서화가 잘 돼 있어 MVP에 적합. 블록 레이어를 직접 짤 때도 저수준 제어가 자유롭다.

**macOS 패키징**: 초반엔 그냥 `cargo run`. 후속으로 `cargo-bundle` 또는 `tauri-bundler`로 `.app` 생성, 이후 codesign/notarization.

## Architecture (4 Layer)

```
┌──────────────────────────────────────────────┐
│  app/       main loop, winit event pump      │  ← bin
├──────────────────────────────────────────────┤
│  ui/        renderer (wgpu), grid layout,    │
│             glyphon text, input handling     │
├──────────────────────────────────────────────┤
│  term/      grid model, VT state machine,    │  ← domain core
│             cells, colors, cursor, scrollback│    (no GPU deps)
├──────────────────────────────────────────────┤
│  pty/       PTY spawn, read/write loop,      │
│             shell env, resize handling       │
└──────────────────────────────────────────────┘
```

핵심 원칙: `term/`은 GPU도 winit도 모름 → 순수 로직, 단위 테스트 용이. `ui/`가 `term::Grid`를 읽어서 그린다. v2에서 블록 모델을 넣을 때 `term/` 위에 `block/` 레이어를 얹는다.

### 데이터 흐름

```
 keyboard → ui::input → pty::writer ──────┐
                                          ▼
                                     child shell
                                          │
 wgpu renderer ◄── term::Grid ◄── vte parser ◄── pty::reader
                       ▲
                       └─ resize events from winit
```

스레드:
1. **Main/UI 스레드** — winit 이벤트 루프, wgpu 렌더링, 키보드/마우스
2. **PTY reader 스레드** — blocking read → `vte::Parser` → `term::Grid` mutation (Mutex 또는 채널 기반 command queue)
3. **PTY writer** — UI 스레드에서 바로 쓰거나 채널로 전달

`term::Grid`는 `Arc<Mutex<Grid>>` 또는 double-buffer. 초기엔 Mutex로 단순하게, 프레임 드롭 보이면 double-buffer로 전환.

## Milestones

Status legend: ✅ done · 🚧 in progress · 🔜 planned

### M0 ✅ Scaffolding
Cargo workspace with four crates (`purrtty-term`, `purrtty-pty`,
`purrtty-ui`, `purrtty-app`), tracing setup, empty winit window that
closes cleanly. Shipped: `e987766`.

### M1 ✅ GPU Text on Screen
wgpu + glyphon initialized on the window surface, "hello purrtty"
drawn at a fixed position, resize and HiDPI handling. Shipped:
`899bc38`.

### M2 ✅ Grid Model + VT Parser (basic)
`purrtty-term` implements:
- `Cell` (`char`, `fg`/`bg` as `Default`/`Indexed`/`Rgb`, `Attrs`
  bitflags: Bold/Dim/Italic/Underline/Reverse/Hidden/Strike), `Pen`
  for current SGR state
- `Grid`: row-major cells, `Cursor`, 10k-row scrollback ring
- `Terminal` wraps `vte::Parser`; Perform impls cover:
  - `print`, `execute` for CR/LF/BS/TAB
  - CSI `H`/`f` (CUP), `J` (ED), `K` (EL), `m` (SGR) with full color
    (8/bright/256/truecolor) and attrs
- Wide-character tracking via `unicode-width`: CJK/emoji advance the
  cursor by 2, right-hand cell is a `WIDE_CONT` sentinel.

16 unit tests. Shipped: `83b1179`, `497b233`.

> **Note:** M2's explicit VT scope (CUP/ED/EL/SGR only) turned out to
> be insufficient for M3's verification goal of "vim/htop works". That
> gap is closed in **M3.5** below rather than by retroactively
> expanding M2.

### M3 🚧 PTY + Shell (basic integration)
`purrtty-pty::PtySession` opens a PTY, spawns `$SHELL` (fallback
`/bin/zsh`) with `TERM=xterm-256color`/`COLORTERM=truecolor`, runs a
background reader thread that calls a caller-supplied callback, and
exposes `write` / `resize`.

`purrtty-app` wires it together:
- `Arc<Mutex<Terminal>>` shared between UI thread and PTY reader
- `EventLoop<UserEvent>` + `EventLoopProxy` for reader→UI wake-up
- `key_event_to_bytes`: Named-key escape sequences for
  Enter/Tab/Backspace/Escape/arrows/Home/End/Delete/PgUp/PgDn, with
  `KeyEvent.text` as fallback for printable input and Space
- `WindowEvent::Resized` propagates to renderer → grid → pty

Renderer updated to take `&Grid` and build the frame text by walking
`row_at(view_idx, scroll_offset)`, skipping `WIDE_CONT` cells. Wrap
disabled on the glyphon buffer so grid rows map 1:1 to visual lines.

**Bonus (originally M4-ish territory):** scrollback view with
mouse-wheel scrolling and snap-back on keyboard input.

**What works now:** zsh prompt, typing (incl. space), simple commands
(`ls`, `pwd`, `echo`, `cat`), long output + wheel scroll, Korean input
preserving prompt integrity (logical widths), resize.

**What's broken:** vim, htop, less, and anything else that uses
IL/DL/scroll-region/alt-screen — handled in M3.5. Color, cursor, Ctrl
combos, and IME stay for M4.

Shipped so far: `35d560e` (wiring), `75af0a2` (space + wrap),
`dba895c` (scrollback), `497b233` (wide chars).

### M3.5 🔜 VT coverage expansion (vim / htop hardening)
Close the gap between M2's explicit VT scope and M3's vim/htop
verification criterion. Additive to `purrtty-term`, no API changes
above it.

**CSI handlers to add:**
- Cursor motion: `A`/`B`/`C`/`D` (CUU/CUD/CUF/CUB), `G` (CHA),
  `d` (VPA)
- Line ops: `L` (IL), `M` (DL), `S` (SU), `T` (SD)
- Character ops: `@` (ICH), `P` (DCH), `X` (ECH)
- `r` — DECSTBM scroll region (top/bottom margins; LF/SU/SD/IL/DL
  all clamp to the region)
- `s`/`u` — cursor save/restore (SCOSC/SCORC)

**ESC dispatch to add:**
- `ESC 7` / `ESC 8` — DECSC/DECRC cursor save/restore

**DEC private modes (`\e[?N h`/`\e[?N l`):**
- `1049` — alt screen buffer: swap to a secondary `Vec<Cell>` and
  save/restore cursor on enter/exit
- `25` — cursor visibility (track state; renderer uses it in M4)
- `7`, `2004`, `1000`/`1002`/`1006` — silently accepted no-ops for
  now (autowrap, bracketed paste, mouse tracking — real support
  later)

**OSC:**
- `0`/`1`/`2` — window title set. Accept silently for now, wire to
  `Window::set_title` as a polish item.

**Grid changes:**
- `alt_cells: Option<Vec<Cell>>` back-buffer; scrollback is primary-
  only (alt screen never goes to scrollback)
- `scroll_top: usize`, `scroll_bot: usize` (half-open) for DECSTBM
- All vertical-scroll paths (`line_feed`, SU/SD, IL/DL at region
  bottom) honor the region
- `saved_cursor: Option<Cursor>` plus pen save/restore

**Verification:**
- Unit tests per added sequence (IL/DL with scroll region, alt
  screen swap, cursor save/restore)
- Manual: `vim test.txt` (insert line / delete line / `:q` leaves the
  shell intact), `htop` (live refresh without artifacts), `less`
  (pgup/pgdn), `tmux` (enter/exit alt screen cleanly)

### M4 🔜 UI polish
- **Per-cell positioning.** Render each cell at `(col * cell_w,
  row * line_h)` instead of relying on cosmic-text flow. Fixes the
  wide-char visual drift and enables exact colored backgrounds.
- **Real monospace metrics.** Measure a reference glyph ("M") via
  cosmic-text instead of the hard-coded `CELL_WIDTH = 10.0`.
- **SGR → pixels.** Per-glyph `glyphon::Color` from `Cell.fg` + a
  wgpu quad pass for `Cell.bg` + attrs (bold via font weight,
  reverse by swapping fg/bg, underline/strike as extra quads).
- **Cursor rendering.** Block and underline styles, blink timer
  driven by winit `set_wait_timeout`, honor the DEC 25 visibility
  flag from M3.5.
- **Modifier keys.** Track `ModifiersState`, map Ctrl+letter →
  `0x01–0x1A`, Ctrl+[/Ctrl+] → `\x1b`/`\x1d`, Alt+letter → ESC +
  letter. Cmd-based shortcuts (Cmd+C/V/+/-) handled in app before
  reaching pty.
- **IME composition.** Forward `WindowEvent::Ime` preedit to a
  temporary overlay; commit inserts bytes via pty.

### M5 🔜 Polish & packaging
- `~/.config/purrtty/config.toml` (font family/size, color scheme,
  initial window size)
- Two default color schemes (light/dark)
- README screenshot
- `cargo-bundle` → unsigned `.app`
- **v0.1 종료 조건**: 일상적인 쉘 작업 + vim + htop 을 purrtty만으로
  문제 없이 할 수 있다.

## Critical Files (to be created)

```
purrtty/
├── Cargo.toml                    # workspace
├── rust-toolchain.toml
├── README.md
├── .gitignore
├── crates/
│   ├── purrtty-term/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── cell.rs           # Cell, Attrs, Color
│   │       ├── grid.rs           # Grid + scrollback
│   │       └── parser.rs         # vte::Perform impl
│   ├── purrtty-pty/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs            # PtySession, reader/writer
│   ├── purrtty-ui/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── renderer.rs       # wgpu device/surface
│   │       ├── text.rs           # glyphon integration
│   │       └── input.rs          # key → bytes
│   └── purrtty-app/
│       ├── Cargo.toml
│       └── src/main.rs           # winit loop, wires everything
└── config.example.toml
```

## Verification (per milestone)

- **M1**: `cargo run -p purrtty-app` → 창 + 텍스트 확인 (수동)
- **M2**: `cargo test -p purrtty-term` — ANSI 시퀀스별 grid 상태 유닛 테스트
- **M3**: `cargo run` 후 `echo hello`, `ls`, `pwd`, `cat` 수동 검증 + 휠
  스크롤 동작 + 한글 입력/삭제
- **M3.5**: `cargo test -p purrtty-term` (IL/DL/scroll region/alt screen
  unit tests) + `vim test.txt` (편집 후 `:q` 가 쉘로 깔끔히 복귀),
  `htop` (찌꺼기 없이 refresh), `less README.md` (pgup/pgdn)
- **M4**: `nvim`에서 구문 하이라이트 색상이 보임, 커서가 보이고 깜빡임,
  창 리사이즈 시 쉘 리사이즈 (`stty size`), 한글 IME 조합
- **M5**: `cargo bundle --release` → 생성된 `.app` 더블클릭 실행 성공

전반적으로 v0.1 은 **통합 테스트보다 수동 스모크 + term 크레이트 유닛 테스트**에 의존. `term/` 이 순수 로직이라 테스트 커버리지 집중 지점.

## Known Issues (discovered during QA, tracked by milestone)

Bugs surfaced during QA of a shipped or in-progress milestone, with
the milestone they're slated to be fixed in.

### Fix in M3.5 (VT hardening)
- **vim / htop / less visibly broken.** Missing IL/DL, scroll region,
  alt screen, and cursor save/restore sequences mean full-screen TUI
  apps leave garbage on the screen, overwrite lines instead of
  inserting, and don't restore the shell on exit. Full list of missing
  sequences is in the M3.5 milestone section above.

### Fix in M4 (UI polish)
- **Wide-char visual misalignment.** `Grid::put_char` tracks CJK/emoji
  glyphs as 2 cells (logical fix in `497b233`), so backspace/erase
  math is correct. The renderer still feeds one big string into
  cosmic-text and trusts its proportional layout, so a Korean glyph
  occupies the font's natural advance (~1.5–2× Latin) rather than
  exactly 2 × `CELL_WIDTH`. Latin and CJK drift apart within a line.
  Fix by switching to per-cell positioning in M4.
- **No visible cursor.** `Cursor` is tracked but not drawn. M4 adds a
  block/underline cursor with blink, honoring DEC mode 25 (which is
  parsed in M3.5 but takes effect visually in M4).
- **All text is monochrome.** SGR state is stored per cell but the
  renderer draws everything at a single default color. M4 converts
  `Cell.fg/bg/attrs` into per-glyph `glyphon::Color` + background
  quad pass.
- **Ctrl / modifier key combos unverified.** Ctrl+C, Ctrl+D, Ctrl+L
  etc. rely on whether winit populates `KeyEvent.text` with the
  control byte on macOS. M4 reads `ModifiersState` explicitly and
  maps Ctrl+letter → `0x01–0x1A`.
- **No Korean IME composition.** Single code points go through
  (wide-char logical fix), but macOS IME preedit/commit isn't wired
  up. M4 forwards `WindowEvent::Ime`.

## Out of Scope for v0.1 (v2+ Backlog)

- Block 기반 커맨드 모델 (Warp 시그니처)
- 탭, 분할 창
- Command Palette
- AI (`#` 자연어, 에러 설명)
- 구문 하이라이트 입력 라인 에디터 (그냥 shell readline 씀)
- Ligature 세밀 제어, IME (한글 조합 — 기초 지원은 winit IME 이벤트로, 고도화는 나중)
- Windows/Linux 포팅 (코드는 크로스플랫폼 지향하되 macOS만 타겟)
- Notarization/배포 파이프라인

## Open Questions (v0.1 도중 결정)

1. **폰트 기본값** — SF Mono(시스템) vs JetBrains Mono(번들)?
   bundle size vs 즉시 사용 가능성 트레이드오프. M5에서 결정.
2. **Grid mutation 동기화** — 현재 `Arc<Mutex<Terminal>>` 로 M3에서
   돌고 있음. 체감상 문제 없음. M4에서 60fps 유지 여부 프로파일링 후
   double-buffer 전환 여부 재판단.
3. **Alt screen에 scrollback?** xterm 계열은 alt screen에서 스크롤백을
   비활성화함. tmux 사용자는 그걸 기대하지만, Warp는 alt screen 내용도
   블록으로 기록함. v0.1은 xterm 관례(alt screen = no scrollback)를
   따르고 M3.5에서 구현.
