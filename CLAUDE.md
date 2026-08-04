# CLAUDE.md

zyrisd — a daemon installed on the user's machine. Comes up at boot, attaches to Attacca as a
Zyris node, serving its terminal, files, and — if a display exists — screen and input.

The "why" of the design is all in `docs/superpowers/specs/2026-08-04-zyris-daemon-design.md`.
This file does not summarize it; it records only **what you need to know before touching code**.

## Why there are two workspaces

This repo holds two Cargo workspaces, and that split is the most important thing about it.

| What | Where | Links graphics |
|---|---|---|
| `zyrisd` (bin), `zyrisd-node`, `zyrisd-display-proto` | root workspace | ✗ |
| `zyrisd-display` (bin) | `display/` — **its own workspace root** | ✓ |

**The unit of Cargo feature unification is not the crate, it is every workspace member pulled
into one cargo invocation.** In one workspace, `cargo build --workspace` also unifies `zyris-capkit`'s
`desktop` feature, so X11/Wayland/mesa/pipewire get stamped into the parent binary as `DT_NEEDED`.
Then on a headless machine that lacks those `.so`s, it dies before it ever reaches `main`.

So there are always two builds:

```bash
cargo build -p zyrisd -j2                                 # parent
cargo build --manifest-path display/Cargo.toml -j2        # child
```

`cargo build --workspace` and `cargo test --workspace` are **never used.**
Confirm the parent links no graphics with:

```bash
readelf -d target/release/zyrisd | grep -Ei 'X11|wayland|xcb|EGL|pipewire'   # must print nothing
```

## This machine's limits

RAM 3.6GB / 4 threads. **Always `cargo` at `-j2` or lower.** The desktop child builds zbus, pipewire
bindgen and wayland-scanner, which makes it especially heavy.

## Traps

- **Do not run `cargo fmt`.** There is no `rustfmt.toml` and the code is written at 96-104 columns.
- **Build the `PtyTerminal` instance once and never build another.** The session cap is per instance
  and the reaper sweeper holds a `Weak`, so dropping it wipes every open PTY session. That is why
  attaching/detaching the desktop capability uses only `Capabilities::add`/`remove`, never `replace()`
  with a fresh instance.
- **We do not delegate to upstream `exec`.** `PtyTerminal::exec` spawns the child inside itself and
  returns only what `cmd.output()` drained, so nothing outside can kill the process group.
  `GatedTerminal::exec` is hand-written instead.
- **`Credentials::refresh()` must call `Enroller::force_refresh`.** Dropping only the in-memory copy
  leaves the store intact, so the dead token goes out again and the runner folds the next 401 to `Refused`.
- **Do not fold a `Transport` error from `force_refresh` into `NeedsOperator`.** One 503 during a
  server deploy becomes exit code 2, and `RestartPreventExitStatus=2` permanently stops every
  daemon that happened to be reconnecting at that moment.
- **Logs go to stderr.** `zyrisd enroll` prints its enrollment code block on stdout; mixing the two
  wrecks the one screen a human actually has to read.
- **Credential and config paths are literal `$HOME`.** The idiomatic `XDG_CONFIG_HOME` lookup
  breaks: the systemd user manager has no such variable, so `enroll` (login shell) and `run` (the unit)
  can end up reading different files.
- When bumping upstream `zyris`, `./scripts/check-zyris-pin.sh` has to pass.

## Commands

```bash
cargo build -p zyrisd -j2
cargo test -p zyrisd-node -j2
cargo clippy -p zyrisd -j2 --all-targets
./scripts/check-zyris-pin.sh
```

## Commits

**There is no remote.** Local commits for backup only, never pushed.
