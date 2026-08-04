# zyrisd design

Written 2026-08-04. Target is `/home/ruma/zyris-daemon`, language is Rust.

---

## 1. What we are building

`zyrisd` is an always-on daemon installed on the user's machine that comes up at boot. It attaches
to Attacca over the Zyris protocol and offers that machine's terminal and files as capabilities.

The functionality itself is already mostly covered by `zyris-hello` in `attacca-cc/zyris`. What this
project adds is **the rest it takes to be a product** — install, residency, enrollment split, boundary
policy, and saying so and stopping quietly when a human has to step in.

**zyrisd cares about itself only.** It does not relay its Attacca connection to other Zyris programs.

## 2. What is decided

| | Decision |
|---|---|
| capability | `terminal` + `file_io` always. `screen_capture`/`input` only when a display is real |
| v1 platform | Linux complete. Windows/macOS get the abstraction only, bodies `unimplemented!` |
| enrollment | Foreground `zyrisd enroll` only. The daemon never enrolls on its own |
| credential revoked | Best-effort desktop notification, then exit code 2. systemd stops restarting |
| `file_io` scope | `roots`/`deny` in `config.toml`. Default is the whole home directory |
| distribution | Assumes GitHub Releases, base URL is a variable |
| desktop | Child-process helper. Package, node, service and enrollment each stay singular |
| requested scopes | None (zero) |

## 3. Repository layout

A Cargo workspace. **Crates are split on one axis: does it link graphics libraries.** That is this
project's dominant constraint, so the crate boundary must be exactly that boundary.

| Crate | What | Links graphics |
|---|---|---|
| `zyrisd` (bin) | CLI dispatch, config loading, install/uninstall, `main`, signals, exit codes | ✗ |
| `zyrisd-node` (lib) | Node assembly — credentials, gating, Runner, ConnSlot, desktop proxy | ✗ |
| `zyrisd-display-proto` (lib) | Parent↔child messages and framing. Depends on serde only | ✗ |
| `zyrisd-display` (bin) | The desktop child process | **✓** |

`zyrisd` **does not depend on** `zyrisd-display`. Cargo features unify per crate, so if parent and
child sit in one crate the `zyris-capkit/desktop` the child turns on links into the parent too. Then
`libX11`, `libwayland`, mesa and pipewire SONAMEs get baked in as `DT_NEEDED`, and **on a headless
machine without those `.so` files it dies before reaching `main`.** Split the crates and invoke the
child only through `Command`, and the same situation becomes a recoverable "process failed to start".

### The zyris dependency

Declare it once in the root `[workspace.dependencies]` and **pin an explicit rev**. Member crates use
only `workspace = true`. Declaring it directly bypasses the pin and links two copies of one crate,
which yields "same type but a different type" errors.

```toml
# 75f7e5d = origin/main HEAD (2026-08-04). Verified all six of these exist at this rev:
#   Capabilities::add / remove, Connection::is_closed, FileCredentialStore::at,
#   Enroller::force_refresh, CredentialStore.
REV = "75f7e5d0f98baa1f72c63c762fb8e577d4b0638e"

zyris       = { git = "https://github.com/attacca-cc/zyris", rev = REV, features = ["enroll"] }
zyris-proto = { git = "https://github.com/attacca-cc/zyris", rev = REV }
zyris-caps  = { git = "https://github.com/attacca-cc/zyris", rev = REV }
zyris-capkit= { git = "https://github.com/attacca-cc/zyris", rev = REV }   # zyrisd-display only
```

(`REV` is written for readability, the real `Cargo.toml` spells the SHA out on all four lines.
`check-zyris-pin.sh` checks that those four lines and `Cargo.lock` agree on the SHA.)

**We do not use `EnrollmentUi` from the `feat/enrollment-ui` branch.** It would pin the rev to an
unmerged branch and buys nothing — `zyrisd enroll` is a foreground command on a TTY, so the block
upstream prints to stdout is exactly right.

We do not use `branch = "main"`. In a product that ships binaries, yesterday's build and today's
must not differ, and branch tracking has no guard to attach. `scripts/check-zyris-pin.sh` checks the
four declarations and `Cargo.lock` see the same rev, and that no member crate declares it directly.

The `enroll` feature is required. Even though the daemon never enrolls, the only public API that
reaches the refresh endpoint is `Enroller::force_refresh`, so an `Enroller` instance must exist.

## 4. Command surface

One binary with subcommands. systemd calls exactly one of them: `zyrisd run`.

| Command | What |
|---|---|
| `zyrisd enroll` | Enroll. Foreground only. Shows a code and waits for approval |
| `zyrisd run` | The daemon. Uses stored credentials only |
| `zyrisd install` | Drop the unit + `enable-linger` + `enable --now`. Idempotent |
| `zyrisd status` | Enrolled or not, service state, capabilities currently announced |
| `zyrisd uninstall` | Remove the unit. Credentials are erased only with `--purge` |

Splitting `enroll` from `run` is not a design choice but a consequence. Use upstream `DeviceGrant`
as-is on the daemon path and every boot **prints the enrollment code to stdout (= the journal)**, blocks
for up to 3 × `expires_in` (~30 min), then dies. It uses `println!`, not `tracing`, so `RUST_LOG` can't hide it.

## 5. Node assembly

### 5.1 Credentials

`zyrisd-node` implements `zyris::runtime::Credentials` itself. No zyris change is needed — every
piece it needs is `pub`.

```
bearer():
  if a valid access token is held in memory, return it
  otherwise store.load()
    Ok(None)                   → NeedsOperator("Not enrolled. Run zyrisd enroll")
    Ok(Some(c)) & valid        → hold it and return
    Ok(Some(c)) & expired      → enroller.force_refresh(&c)
                                   Some(new) → return it
                                   None      → NeedsOperator("Credentials were revoked")
    Err(discardable)           → NeedsOperator (corrupt file: tell them to re-enroll)
    Err(not discardable)       → NeedsOperator (permission problems and the like)

refresh():   drop what is held and Ok(true).
             the next bearer() finds an empty store and ends in NeedsOperator.
```

**`force_refresh` never starts an enrollment.** If the server refuses the refresh it simply clears
the store itself and returns `Ok(None)`. So no path in this implementation prints an enrollment code.

Mapping store errors to **retriable** gives an endless restart loop. Anything a human has to fix —
a world-readable credentials file, say — must be `NeedsOperator`.

#### Credentials file path

**Name the path explicitly** with `FileCredentialStore::at()`: `~/.config/zyrisd/credentials.json`
(`credentials-<profile>.json` when the profile is not `default`).

Three reasons not to use `with_file_store()`.

- It builds the filename as `slug(url)-slug(profile).json`, and **the slug truncates at 48 chars.**
  Two internal URLs sharing their first 48 chars collide on the same file.
- The path derives from `ZYRIS_CONFIG_DIR`, `ZYRIS_SERVER_URL` and `ZYRIS_PROFILE`, so if any one of
  the three is spelled differently at `enroll` time than when the unit `run`s, the daemon reads a
  different file and decides "not enrolled". Leaving that match to the unit is brittle.
- `ZYRIS_CONFIG_DIR` is used verbatim (no subdirectory appended), yet saving unconditionally chmods
  the parent directory to `0700`. Point it at a shared directory and it silently changes its mode.

Change `server_url` and the old credentials clean themselves up: 401 → `force_refresh` refused →
store deleted automatically → "re-enrollment required".

If credentials already exist, `zyrisd enroll` says up front that it will overwrite them and confirms.

#### Requested scopes

None. `request_scopes([])`. zyrisd is a pure tool provider with no reason to touch the owner's
account, which is why upstream `RunConfig` defaults to empty too. "Requests no account access" on
the approval screen is better for trust as well.

If `ZYRIS_SCOPES` is in the environment it silently defeats `request_scopes` in code. The unit does
not set that variable.

Static `znt_` tokens are not supported in v1. Keep one path.

### 5.2 Gating

`zyrisd-node` puts `GatedFileIo` and `GatedTerminal` around `LocalFileIo`/`PtyTerminal`.
They are `ServeCapability` decorators, so the upstream implementations are used as-is.

**Files** — resolve symlinks first, check the result is inside `roots`, refuse anything in `deny`.
capkit's `resolve_under` allows writes to absolute paths outside the root, and tests pin that contract.
Without this layer, `roots` in the config is decoration.

**exec** — cap output bytes, and on timeout kill **the whole process group**. Upstream caps nothing
on exec output (only PTY reads truncate at 128 KiB) and its timeout does not kill the child, so a
daemon running for weeks piles up zombies.

Say plainly in the docs what this boundary policy is. **As long as `terminal` is offered, the agent
can open any file anyway through a shell.** This gate stops accidents, not intruders. That is why
`deny` defaults to empty — blocking `~/.ssh` by default creates a mismatch where `file_io` fails and
`cat` succeeds, and makes protection that isn't there look real. Document it as an example only.

The `PtyTerminal` instance is **created once and never created again.** The session cap is per
instance and the reaper holds a `Weak`, so dropping it wipes out every open PTY session at once.

### 5.3 Lifecycle and exit codes

```
main ─┬─ tokio::spawn(runner.try_run())      on_connect fills the ConnSlot
      └─ select! { try_run done      → map the exit code
                   SIGTERM | SIGINT  → close() the Connection in the slot → 200ms → exit 0 }
```

Use `try_run()` rather than `Runner::run()` and let zyrisd map the exit code itself.

**SIGTERM.** Upstream `Runner` waits on `tokio::signal::ctrl_c()` (= SIGINT) only, and only while a
connection is alive. systemd stops services with SIGTERM, so left alone the process dies without a
close frame and the server sees this node as online until the heartbeat expires. A signal arriving
while dialing or waiting in backoff reaches nobody, even if it is SIGINT.

zyrisd installs its own handler and takes both signals. `Connection` is `Clone`, `close(&self)` is
synchronous, and `on_connect` hands over the whole `Connection`, so no upstream patch is needed.
The slot is not emptied on disconnect, so **check `is_closed()` before using it.**

`close()` alone just makes `Runner` reconnect, so **we must end the process ourselves.** That is why
`main` cannot simply return the `ExitCode` from `Runner::run()`.

**Exit codes.** Trusting upstream's mapping gives an endless restart loop. `RunError::Refused` means
the server refused this node in a way retries cannot fix, and upstream gives it a 1.

| Code | When | What the unit does |
|---|---|---|
| 0 | Clean exit on SIGTERM/SIGINT | No restart |
| 2 | Not enrolled · revoked · credentials file mode · config error · **`Refused`** | `RestartPreventExitStatus=2` |
| 1 | Everything else | Restart |

Just before exiting 2, if `notify.enabled`, make a best-effort desktop notification (skipped when
`notify-send` or a session is missing). The reason always goes to the log.

Start from `RunConfig::from_env()`, overwrite fields with config-file values, assemble the
credentials, then use `Runner::new(config, credentials)`. **Scopes must be settled before the
`Enroller` is built** — that is exactly why upstream defers credential resolution to `run` time, and
assembling it ourselves loses that guard. `Enroller` does not hand back its own store, so zyrisd
creates the `Arc<dyn CredentialStore>`, passes a clone in, and keeps its own.

## 6. Desktop helper

At startup the parent spawns `zyrisd-display` and sends `Probe`. If it enumerates at least one
display, `capabilities.add(ScreenCaptureServer(proxy))`, and `InputServer` too if input works. When
the child dies we `remove` and retry with backoff (1s→60s). A screen appearing later still attaches.

**Use only `add`/`remove`, never hand a fresh instance to `replace()`.** `add`/`remove` clone the
existing `Arc`s and reassemble, so `PtyTerminal` is never dropped. `replace` with a new instance
kills every open PTY session.

Detection is not delegated to capkit's backend selection. On a headless box that path can report
success without ever trying to connect, and it panics on some compositors. It has to be an **active
probe — "did we actually enumerate at least one display?"** — and it must run inside the child so a
panic cannot kill the daemon. That is the second reason for splitting the child out.

### Protocol

One frame at a time over the child's stdin/stdout:

```
[u32 BE json_len][json][u32 BE blob_len][blob]
```

Requests carry an `id` and go one at a time. On timeout, kill the child and respawn. Screenshot
bytes ride in the blob frame, so there is no base64 bloat. The child exits when stdin closes, so no
orphan is left behind if the parent dies.

Messages: `Probe` → `{displays, input_ok}`; `Screenshot{display, region, format, max_width}` → blob;
`MoveTo`/`Click`/`Key`/`Type`/`Scroll` → ok.

Child lookup order: `$ZYRISD_DISPLAY_BIN` → sibling of `current_exe()` → `PATH`.

## 7. Configuration

`~/.config/zyrisd/config.toml`. If it is missing, everything runs on defaults.

```toml
[node]
name       = "build-box"                       # default: hostname
server_url = "wss://attacca.cc/api/zyris/v1/ws"
profile    = "default"

[files]
roots = ["~"]        # default: the whole home
deny  = []           # e.g. ["~/.ssh"]

[terminal]
max_output_bytes  = 1048576
exec_timeout_secs = 120

[desktop]
enabled = true

[notify]
enabled = true
```

If the config has a syntax error or `roots` names a nonexistent path, **we never come up: exit 2.**
Better that than silently opening wide permissions from a bad config.

## 8. Install and packaging

Approval takes a human, so **"works right after install" is impossible.** Design around three steps.

```
① install binary   .deb or install.sh
② zyrisd enroll    show the code, a human approves
③ zyrisd install   drop unit + enable-linger + enable --now
```

### The systemd user unit

A system unit does not work. Three independent reasons reach the same conclusion.

- The PTY shell inherits the daemon's whole environment and the default shell is the process's
  `$SHELL`, but the system manager's environment has no `SHELL` and no `DBUS_SESSION_BUS_ADDRESS`.
- `/run/user/<uid>` is `0700`, so a service under another uid simply cannot reach the user session.
- Credentials live in a `0600` file in the user's home and loading refuses when group/other bits are
  set, so the enroll command and the daemon must run as the same uid.

```ini
[Unit]
Description=Zyris daemon

[Service]
Type=simple
ExecStart=/absolute/path/zyrisd run      # zyrisd install fills this from current_exe()
Restart=on-failure
RestartSec=5
RestartPreventExitStatus=2
KillSignal=SIGTERM
TimeoutStopSec=10

[Install]
WantedBy=default.target
```

`WantedBy=default.target`, and `loginctl enable-linger` is **a required install step**. Without
linger the user manager only comes up at login, so "starts at boot" does not hold.

The unit file is **generated by `zyrisd install`, not shipped by the package.** It is written to
`~/.config/systemd/user/` with `ExecStart` filled from the absolute `current_exe()`. If the package
owned the unit, `.deb` (`/usr/bin/zyrisd`) and `install.sh` (`~/.local/bin/zyrisd`) would demand
different `ExecStart`s and the paths could drift. The generator always knows where it lives.

We do not use `After=network-online.target`. In a user unit it is silently ignored, yet
`systemd-analyze --user verify` passes it, so having it fools you into thinking it works. The
runner's 1s→30s backoff already does that job.

Put no secrets in the unit's `Environment=`/`EnvironmentFile=`. The PTY inherits the daemon's whole
environment, so every shell the agent opens sees them verbatim.

### .deb

**It places binaries only.** A .deb cannot know which user to enable linger and the unit for,
and cargo-deb's `[package.metadata.deb.systemd-units]` block is system-unit only, so it is unusable.
The maintainer script only points at the next steps (`zyrisd enroll` → `zyrisd install`).

```
/usr/bin/zyrisd
/usr/libexec/zyrisd-display
```

Graphics libraries go in `Recommends:`, not `Depends:`. The child process exists precisely so that
a headless install is not forced to pull them in; promoting them to `Depends:` throws that away.

`zyrisd install` is idempotent: it writes the unit (left alone if identical), turns on `enable-linger`,
then `enable --now`. If everything is already in place it changes nothing and reports that.

### install.sh

POSIX sh. Fetches the tarball from `$ZYRISD_BASE_URL` (default: GitHub Releases), verifies sha256,
unpacks into `~/.local/bin` and `~/.local/libexec`, then calls `zyrisd install`. It detects OS/arch,
accepts only `x86_64`/`aarch64` Linux, and refuses everything else with a clear message.

Do not decide whether `systemctl --user` works from the exit code of `is-system-running`. One unrelated
degraded unit gets a healthy machine wrong. Decide from the presence of `XDG_RUNTIME_DIR` and
`systemctl --user show -p Version`.

## 9. Tests

The in-memory duplex in `zyris::testing` runs an announce round trip with no server and no DB. Gating,
credentials, and the desktop proxy are all tested in-process.

- **Gating** — absolute paths outside `roots`, symlink escapes, `deny` precedence, exec output cap,
  and whether a timeout kills the process group.
- **Credentials** — empty store gives `NeedsOperator`, expiry rotates through `force_refresh`, a refused
  rotation ends in exit code 2, and a permission error is 2 **and not a retry**.
- **Exit codes** — the whole mapping table. `Refused` → 2 in particular.
- **Desktop** — framing round trip, blob boundaries, `remove` when the child dies, retry backoff,
  and that the `PtyTerminal` instance is unchanged after `add`/`remove`.
- **SIGTERM** — spawn the process, `kill -TERM`, check the close frame goes out (integration test).

Tests go in an inline `mod tests` at the bottom of each source file, names in snake_case, with a doc
comment above saying what it protects. Do not add a `rustfmt.toml`; match nearby line width by eye.

## 10. Non-goals

Brokering connections for other Zyris programs, self-update, concurrent profiles, static `znt_` tokens,
the `browser` capability. Windows/macOS get the `ServiceManager` trait only; impls are `unimplemented!`.

## 11. What stays unverified

The work machine is Arch Linux / GNOME Wayland / 3.6GB RAM. The following **can be built but not
verified here.** The spec does not hide it.

| Item | Why | How it gets confirmed |
|---|---|---|
| real `dpkg -i` install | no dpkg here | Debian/Ubuntu container |
| dash compatibility | `/bin/sh` is a bash symlink | `sh install.sh` in `debian:bookworm-slim` |
| glibc compat of the shipped binary | 2.44 here → 2.36 on bookworm | release build in an old-glibc container (CI) |
| desktop capture actually working | GNOME has no `zwlr_screencopy` | a sway/wlroots or X11 session |
| detection on other desktops | one machine does not generalize | run `Probe` on KDE/sway/X11 |
| the child binary's real `DT_NEEDED` list | no desktop artifact yet | `readelf -d`, then start in a lib-less container |

Build parallelism stays at `-j2` or lower; the default `-j4` eats all RAM at link. The desktop child
builds zbus, pipewire bindgen and wayland-scanner, so it is especially heavy.

## 12. Commit policy

This repo has **no remote.** Local commits for backup only. Nothing is pushed to GitHub.
