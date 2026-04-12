# Block Mode (v0.3)

## Overview

Block mode groups every prompt-command-output cycle into a visually
distinct **block** — a bordered region in the terminal grid. This is
the core Warp UX: the entire terminal experience is blockified, not
just agent interactions.

Each block represents one shell interaction:
1. The shell prints a prompt → a new block begins
2. The user types a command
3. The user presses Enter → the block transitions to "running"
4. The command's output streams into the block
5. The shell prints the next prompt → the current block is complete,
   a new block begins

Agent interactions via `>` are simply blocks where the "command" is
an agent prompt and the "output" is the agent's streamed response.

## Shell Integration (OSC 133)

Detecting block boundaries requires cooperation from the shell. The
industry-standard protocol is **OSC 133** (also called "semantic
prompt" or "shell integration marks"), used by Warp, iTerm2, VS
Code terminal, and kitty.

The shell emits escape sequences at key points:

| Mark | Sequence | Meaning |
|------|----------|---------|
| A | `\e]133;A\a` | Prompt start — new block begins here |
| B | `\e]133;B\a` | Command start — user pressed Enter |
| C | `\e]133;C\a` | Output start — command is running |
| D | `\e]133;D;N\a` | Command done — exit code N |

### Shell configuration (one-time setup)

**zsh** (add to `.zshrc`):
```zsh
precmd()  { print -Pn "\e]133;A\a" }
preexec() { print -Pn "\e]133;B\a\e]133;C\a" }
```

**bash** (add to `.bashrc`):
```bash
PS0='\e]133;B\a\e]133;C\a'
PS1='\e]133;A\a\u@\h \w \$ '
PROMPT_COMMAND='printf "\e]133;D;%s\a" "$?"'
```

**fish**: Supports OSC 133 natively (no config needed on recent
versions).

## Visual anatomy

```
┌─────────────────────────────────────────────┐
│ user@host ~/project $ ls                    │  ← prompt + command
├─────────────────────────────────────────────┤
│ Cargo.toml  src/  tests/  README.md         │  ← output
└─────────────────────────────────────────────┘
┌─────────────────────────────────────────────┐
│ user@host ~/project $ cargo test            │  ← prompt + command
├─────────────────────────────────────────────┤
│ running 37 tests                            │  ← output
│ test result: ok. 37 passed                  │
└──────────────── exit 0 ─────────────────────┘
┌─────────────────────────────────────────────┐
│ user@host ~/project $ █                     │  ← current block (input)
└─────────────────────────────────────────────┘
```

Error blocks (non-zero exit):
```
┌─────────────────────────────────────────────┐  (red border)
│ user@host ~/project $ false                 │
├─────────────────────────────────────────────┤
│                                             │
└──────────────── exit 1 ─────────────────────┘
```

Agent blocks (triggered by `>`):
```
┌─ Agent ─────────────────────────────────────┐  (blue border)
│ > what does this function do?               │
├─────────────────────────────────────────────┤
│ This function parses VT escape sequences... │
│ ⚡ Read src/parser.rs                       │
│   fn csi_dispatch(...) { ...                │
├─────────────────────────────────────────────┤
│ ⠹ Running: Read — 3s                       │  ← live status
└─────────────────────────────────────────────┘
```

## Data model

```rust
/// A single prompt-command-output block.
struct TermBlock {
    /// Absolute grid row where this block starts (prompt line).
    start_row: usize,
    /// Absolute grid row where the next block starts (or current
    /// cursor row if this is the active block).
    end_row: usize,
    /// Lifecycle state.
    state: TermBlockState,
    /// If this block is an agent interaction, holds the agent-
    /// specific state (segments, started_at, etc.)
    agent: Option<AgentBlock>,
}

enum TermBlockState {
    /// Prompt is visible, user is typing (mark A received).
    Input,
    /// User pressed Enter, command running (marks B+C received).
    Running,
    /// Command finished. Holds exit code from mark D.
    Done { exit_code: i32 },
}
```

## Implementation plan

### Step 1 — OSC 133 parsing
- [ ] Add OSC 133 handler in `parser.rs` (`osc_dispatch`).
- [ ] When mark A/B/C/D is received, call a new method on Grid
      (e.g., `grid.mark_block_boundary(mark)`).

### Step 2 — Block tracking in Grid
- [ ] Add `blocks: Vec<TermBlock>` to Grid.
- [ ] On mark A: push a new TermBlock with state=Input.
- [ ] On mark B+C: transition last block to Running.
- [ ] On mark D: transition last block to Done { exit_code }.
- [ ] Track end_row dynamically (updated on every advance).

### Step 3 — Renderer overlay
- [ ] For each block visible in the viewport, draw:
      - Full border (color based on state: gray=done, red=error,
        default=input, blue=agent-active)
      - Background tint
      - Separator line between prompt row and output
      - Footer with exit code for completed blocks
- [ ] Sticky header when block scrolls above viewport.

### Step 4 — Agent block integration
- [ ] When user types `>` and spawns an agent, mark the current
      block as an agent block (set `agent: Some(AgentBlock)`).
- [ ] Agent-specific footer (spinner + tool name + elapsed).
- [ ] StatusTick timer for spinner animation.

### Step 5 — Shell integration setup
- [ ] On PTY spawn, inject OSC 133 hooks into the shell's rc file
      (or use PROMPT_COMMAND / precmd).
- [ ] Or: provide a `purrtty shell-integration` command the user
      can source from their shell config.

### Step 6 — Tests
- [ ] OSC 133 parsing unit tests (A, B, C, D marks).
- [ ] Block boundary tracking on Grid.
- [ ] Block state transitions.
- [ ] Overlay row calculation with scrollback.
- [ ] Agent block footer formatting.

## Graceful degradation

If the shell does NOT emit OSC 133 (no shell integration), blocks
are disabled and the terminal looks like a normal terminal — no
borders, no grouping. The experience is the same as before block
mode. Users opt in by adding the shell integration snippet to
their shell config.

Agent blocks (triggered by `>`) work regardless of OSC 133 because
we control the block boundaries ourselves.

## Out of scope (this pass)

- Collapsible blocks (fold long output).
- Block-level copy (select entire block's output).
- Reordering blocks.
- Block search / filter.
- Inline images inside blocks.
