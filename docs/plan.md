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

### M0 — Scaffolding
- `cargo init` → workspace 구조 (`crates/purrtty-term`, `crates/purrtty-pty`, `crates/purrtty-ui`, `crates/purrtty-app`)
- `Cargo.toml` 루트에 공통 의존성, `rust-toolchain.toml` (stable)
- `.gitignore`, `README.md` (1페이지짜리 — 목적/빌드법만)
- `tracing` 셋업
- 빈 winit 창이 뜨고 Cmd+Q로 닫힘

### M1 — GPU Text on Screen
- `wgpu` 디바이스/서피스 초기화 (Metal 백엔드)
- `glyphon` 으로 고정 문자열 "hello purrtty" 를 렌더
- 윈도우 리사이즈 대응
- HiDPI (Retina) 스케일 팩터 처리
- **검증**: 창에 텍스트가 선명하게 나오고 리사이즈 시 깨지지 않음

### M2 — Grid Model + VT Parser (`term/` 크레이트)
- `Cell { ch: char, fg, bg, attrs }`
- `Grid { cells: Vec<Cell>, cursor: (row,col), size }`
- `vte::Parser` 에 `Perform` 구현 → grid mutation
  - `print(c)`, `execute(CR/LF/BS/TAB)`, `csi_dispatch` (CUP, ED, EL, SGR)
  - SGR은 색/굵기/반전 정도만 v0.1
- Scrollback 버퍼 (링 버퍼, 10k 라인)
- **검증**: 단위 테스트 — ANSI escape 문자열 주입 → grid 상태 assert. `printf '\e[31mRED\e[0m'` 시뮬레이션.

### M3 — PTY + Shell (`pty/` 크레이트)
- `portable-pty` 로 `$SHELL` (없으면 `/bin/zsh`) 스폰
- 환경변수: `TERM=xterm-256color`, `COLORTERM=truecolor`
- Reader 스레드 → `Vec<u8>` → `vte::Parser` → `Grid`
- Writer: 키보드 입력을 바이트로 → PTY
- Winit 리사이즈 → PTY `set_size(rows, cols)`
- **검증**: 창에서 `ls`, `vim`, `htop` 정도가 깨지지 않고 동작

### M4 — UI Integration (`ui/` + `app/`)
- `Grid` → `glyphon::Buffer` 변환 (셀 단위 → 텍스트 런으로 병합)
- 셀 배경색은 별도 wgpu 쿼드 패스
- 커서 렌더 (블록/밑줄, 깜빡임)
- 폰트 사이즈/패밀리 설정 (하드코딩 → toml)
- 기본 키 바인딩: Cmd+C/V (selection 없으니 일단 clipboard 입력만), Cmd+플러스/마이너스 폰트 크기
- **검증**: zsh에서 `htop`, `nvim`, `ls --color` 가 시각적으로 올바르게 보임. 60fps 유지.

### M5 — Polish for v0.1 release
- 설정 파일 (`~/.config/purrtty/config.toml`) — 폰트/색상 스킴/창 크기
- 기본 color scheme 2개 (light/dark)
- README 업데이트 + screenshot
- `cargo-bundle` 로 `.app` 빌드
- **v0.1 종료 조건**: 일상적인 쉘 작업을 purrtty만으로 할 수 있음

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
- **M3**: `cargo run` 후 `echo hello`, `ls`, `vim test.txt`, `htop` 순차 수동 검증
- **M4**: 같은 수동 스모크 + `nvim`에서 구문 하이라이트 색상 정상 확인, 창 리사이즈 시 쉘 리사이즈 동작 (`stty size`)
- **M5**: `cargo bundle --release` → 생성된 `.app` 더블클릭 실행 성공

전반적으로 v0.1 은 **통합 테스트보다 수동 스모크 + term 크레이트 유닛 테스트**에 의존. `term/` 이 순수 로직이라 테스트 커버리지 집중 지점.

## Known Issues (to fix before v0.1 ships)

Bugs surfaced during QA of in-progress milestones that are deferred to
a later milestone in the same release cycle.

### M3 → M4

- **Wide-char visual misalignment.** `Grid::put_char` tracks CJK/emoji
  glyphs as 2 cells (fixed in 497b233), so logical operations like
  backspace are correct. But the current renderer feeds one big string
  into cosmic-text and trusts its proportional layout, so a Korean glyph
  occupies the font's actual advance width (~1.5–2× Latin advance)
  rather than exactly 2 × `CELL_WIDTH`. Latin and CJK drift apart
  across a line. Fix in M4 by switching to per-cell positioning: emit
  each cell at `col * CELL_WIDTH` instead of relying on text flow.

- **No visible cursor.** The grid tracks `Cursor { row, col }` but the
  renderer draws nothing at that position. M4 adds a block/underline
  cursor (with blink) rendered as a wgpu quad behind/under the cell
  glyph.

- **All text is monochrome.** SGR color/attribute state is stored per
  cell but the renderer draws everything at a single default color.
  M4 turns `Cell.fg/bg/attrs` into per-glyph `glyphon::Color` + a
  separate background-quad pass.

- **Ctrl / modifier key combos unverified.** Ctrl+C, Ctrl+D, Ctrl+L
  etc. may or may not work depending on whether winit populates
  `KeyEvent.text` with the control byte on macOS. M4 should handle
  modifiers explicitly (read `ModifiersState`, convert Ctrl+letter to
  bytes 0x01–0x1A).

- **No Korean IME composition.** Single code points go through fine
  (wide-char fix above) but the preedit / commit dance from macOS IME
  isn't wired up. M4 or M5 depending on complexity — winit emits
  `WindowEvent::Ime` events we can forward.

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

1. 폰트 기본값 — SF Mono vs JetBrains Mono 번들? (bundle size vs UX)
2. 한글 IME — 초반부터 붙일지, M5 폴리시에서 할지
3. Grid mutation 동기화 — Mutex vs 더블 버퍼 (우선 Mutex, 프로파일링 후 결정)
