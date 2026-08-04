# zyrisd design

Written 2026-08-04. Target is `/home/ruma/zyris-daemon`, language is Rust.

> **Revision 2 (2026-08-04)** — Checked the draft's claims against the source at pinned rev `75f7e5d`;
> fixed 6 blockers, 20 majors. Key fixes: splitting crates alone won't stop graphics linking (§3),
> gating can't be done with decorators alone (§5.2), `refresh()` does not clear the store (§5.1),
> child-binary lookup fails under every install layout (§6), and the automatic install path skips
> enrollment and freezes the unit in failed (§8).

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

**Split into two workspaces.** The least intuitive decision in this design, and it has one reason.

Cargo unifies features not per crate but across **every workspace member selected by a single cargo
invocation** (resolver 3 included). If `zyrisd-node` and `zyrisd-display` sit in one workspace and
both depend on `zyris-capkit`, one `cargo build --workspace` unifies capkit's `desktop` feature and
**links X11/Wayland/mesa/pipewire into the parent binary too.** The compile then fails outright on a
headless build machine, and the `.deb`'s `Recommends:` strategy collapses. Splitting crates does not
stop it — the build invocations must be split, and splitting workspaces makes that mistake impossible.

```
Cargo.toml                     [workspace] members = ["crates/*"], exclude = ["display"]
crates/zyrisd/                 (bin) CLI, config, install/uninstall, main, signals, exit codes
crates/zyrisd-node/            (lib) node wiring — credentials, gating, Runner, ConnSlot, desktop proxy
crates/zyrisd-display-proto/   (lib) parent↔child messages and framing. serde only
display/Cargo.toml             [workspace] its own root — separate Cargo.lock
display/                       (bin) zyrisd-display. the only place graphics get linked
```

`display/` refers to `crates/zyrisd-display-proto` as a path dependency (Cargo allows path deps that
point outside the workspace). The build runs twice:

```bash
cargo build -p zyrisd                              # parent. no graphics dev packages needed
cargo build --manifest-path display/Cargo.toml     # child. graphics dev packages required
```

`zyrisd-node` depends on `zyris-capkit` **with default features** (`LocalFileIo` and `PtyTerminal` sit
behind the default `file-io`/`terminal` features). Only `display/` turns on `desktop`.

### The zyris dependency

Declare it once in each workspace root's `[workspace.dependencies]` and **pin the rev explicitly.**
Member crates use `workspace = true` only. Declaring directly bypasses the pin and links two copies of
the same crate, which gives "same type but different types" errors.

```toml
# rev = 75f7e5d0f98baa1f72c63c762fb8e577d4b0638e  (= origin/main HEAD, 2026-08-04)
# Confirmed present at this rev:
#   Capabilities::{add, add_arc, remove, replace} · Connection::is_closed
#   FileCredentialStore::at · Enroller::force_refresh · CredentialStore
zyris        = { git = "…/zyris", rev = "75f7e5d…", features = ["enroll"] }
zyris-proto  = { git = "…/zyris", rev = "75f7e5d…" }
zyris-caps   = { git = "…/zyris", rev = "75f7e5d…" }
zyris-capkit = { git = "…/zyris", rev = "75f7e5d…" }

[dev-dependencies]
# The SIGTERM integration test needs a real socket — no separate process can join a duplex.
zyris = { git = "…/zyris", rev = "75f7e5d…", features = ["enroll", "axum"] }
```

Do not turn off `default-features`. `persistence` is a default feature of zyris, and without it
`FileCredentialStore` disappears.

We do not use `branch = "main"`. In a product that ships binaries, yesterday's build and today's
differ, and branch tracking gives you nothing to guard. `scripts/check-zyris-pin.sh` checks **both**
workspaces: that the declaration and `Cargo.lock` point at the same rev.

The `enroll` feature is required. The daemon never enrolls, but the only public API that **refreshes
without starting an enrollment** is `Enroller::force_refresh`, so an `Enroller` instance must exist.
(`obtain()` also reaches refresh, but an empty store falls into enrolling — no use to the daemon.)

## 4. Command surface

One binary with subcommands. systemd calls exactly one of them: `zyrisd run`.

| Command | What |
|---|---|
| `zyrisd enroll` | Enroll. Foreground only. Shows a code and waits for approval |
| `zyrisd run` | The daemon. Uses stored credentials only |
| `zyrisd install` | write the unit + `enable-linger` + `enable --now` + confirm it stays up |
| `zyrisd status` | enrolled or not, service state, announced capabilities, absolute paths in use |
| `zyrisd uninstall` | removes the list in §8 below. credentials and config only with `--purge` |

`enroll` and `install` **refuse explicitly under uid 0.** This is a user-session service, and enrolling
as root leaves the credentials in `/root`, where the daemon will never find them.

Splitting `enroll` from `run` is not a design choice but a consequence. Use upstream `DeviceGrant`
as-is on the daemon path and every boot **prints the enrollment code to stdout (= the journal)**, blocks
for up to 3 × `expires_in` (~30 min), then dies. It uses `println!`, not `tracing`, so `RUST_LOG` can't hide it.

### The `zyrisd enroll` sequence

`Enroller::obtain()` **returns the stored credentials as-is and skips enrollment while they are valid.**
So "ask before overwriting" alone never re-enrolls. Spell the order out.

```
1. flock the credential directory (keeps run from racing)
2. store.load() — if present, show owner and node name and confirm the re-enrollment
3. stop the unit if active  (so a running daemon does not rotate on the old token)
4. store.clear()
5. enroller.obtain()  — the code appears here and a human approves
6. if the unit is installed, systemctl --user reset-failed && restart
```

Without step 6, **the very path where the product called for a human has no way back.** A unit stopped
at exit 2 on revoked credentials is frozen failed by `RestartPreventExitStatus=2`, and even after
re-enrolling, nobody turns it back on.

## 5. Node assembly

### 5.1 Credentials

`zyrisd-node` implements `zyris::runtime::Credentials` itself. No change to zyris is needed.

```
bearer():
  return the held access token if still valid (stored.bearer(now_unix(), 30))
  otherwise store.load()
    Ok(None)                        → NeedsOperator("Not enrolled. Run zyrisd enroll")
    Ok(Some(c)) & bearer(now,30) ok → hold and return
    Ok(Some(c)) & expired           → rotate(c)
    Err(Corrupt | UnknownVersion)   → NeedsOperator("Credential file is corrupt. Re-enroll")
    Err(Permissive | NoConfigDir)   → NeedsOperator (permissions/path: a human has to fix it)
    Err(other Io)                   → Unavailable  (EACCES/EIO/ESTALE etc: retry with backoff)

rotate(c):
  enroller.force_refresh(&c)
    Ok(Some(new))                   → store, hold, return
    Ok(None)                        → the store is already cleared. gave_up = true.
                                      NeedsOperator("Credentials were revoked")
    Err(Transport)                  → Unavailable  (server unreachable. credentials may be fine)
    Err(other)                      → NeedsOperator

refresh():                          ← the runner calls this once after a 401
  nothing held, Ok(false)
  otherwise rotate(held)
    success                         → Ok(true)   (worth dialing again)
    Ok(None)                        → Ok(false)  (gave_up = true)
    Err(Transport)                  → Unavailable (the rotation never actually happened)
```

**The point is that `refresh()` calls `force_refresh`.** Dropping only the in-memory copy leaves the
store intact, the next `bearer()` returns the same dead token, and `rotated_after_refusal = true` is
already set, so the second 401 becomes `RunError::Refused` — store not cleared, no re-enroll prompt.
Clock drift on resume from suspend, or a redeploy that invalidates only access tokens — **every case
that can heal itself with the refresh token** is decided here.

**Never fold `Err(Transport)` into `NeedsOperator`.** A single 503 during an Attacca redeploy becomes
exit code 2, and `RestartPreventExitStatus=2` **halts every zyrisd reconnecting right then, for good.**
Upstream guards this with a dedicated test ("an outage must never unenroll a node").

#### Credentials file path

Set the path explicitly with `FileCredentialStore::at()`. **The literal
`$HOME/.config/zyrisd/credentials.json`** — `XDG_CONFIG_HOME` is ignored. That variable is missing from
the systemd user manager env, so an idiomatic impl lets `enroll` (login shell) and `run` (unit) diverge.
`zyrisd status` prints the absolute path it is actually reading.

Three reasons not to use `with_file_store()`.

- It builds the filename as `slug(url)-slug(profile).json`, and **the slug truncates at 48 chars.**
  Two internal URLs sharing their first 48 chars collide on the same file.
- The path derives from `ZYRIS_CONFIG_DIR`, `ZYRIS_SERVER_URL` and `ZYRIS_PROFILE`, so if any one of
  the three is spelled differently at `enroll` time than when the unit `run`s, the daemon reads a
  concludes it is not enrolled.
- `ZYRIS_CONFIG_DIR` is used verbatim (no subdirectory appended), yet saving unconditionally chmods
  the parent directory to `0700`. Point it at a shared directory and it silently changes its mode.

**No profiles in v1.** The file path is fixed to one, and multiple profiles are a non-goal in §10.

Change `server_url` and old credentials go 401 → `refresh()` → `force_refresh` refused → store wiped →
"re-enrollment required" — that is the recovery.

Both `run` and `enroll` take a `flock` on the credential directory. Upstream saves by atomic rename, but
the temp path is fixed (`path.with_extension("tmp")`), so concurrent saves race.

#### Requested scopes

None. **Always pass `vec![]` to `Enroller::new`.** `Runner::request_scopes` only changes `config.scopes`,
and on the path that passes our own `Credentials` (`CredentialSource::Given`) it only reaches the log
and never reaches `Enroller` — the argument to `Enroller::new` is the only thing that sets scopes.

zyrisd is a pure tool provider with no reason to touch the owner's account, and "requests no account
access" on the approval screen is better for trust too. The unit does not set `ZYRIS_SCOPES` (setting it
silently defeats `request_scopes` in the code).

Static `znt_` tokens are not supported in v1.

### 5.2 Gating

**The layer to wrap with a decorator is the `FileIo`/`Terminal` trait, not `ServeCapability`.**
Only the macro-generated `FileIoServer<T>`/`TerminalServer<T>` implement `ServeCapability`, while
`LocalFileIo`/`PtyTerminal` implement only `FileIo`/`Terminal`. So zyrisd writes its own
`GatedFileIo`/`GatedTerminal` over those traits, keeps the upstream impl inside, and wraps it in
`FileIoServer(GatedFileIo{..})`.

#### There is exactly one root

Upstream takes **exactly one** root — `LocalFileIo::rooted(root)` is the only constructor, and
`PtyTerminal::rooted(root)` is single too. That one root fixes (a) the base for relative paths, (b) the
PTY shell's starting directory, and (c) exec's default cwd, all at once.

So pass `roots[0]` as that value and **use the remaining roots purely as an absolute-path allowlist.**

#### Path checks

`resolve_under` allows absolute paths outside the root, fixed in three places (the doc "the root is
a default, not a jail", unit tests, integration tests). Without it, config `roots` is decoration.

Upstream has no symlink resolution at all, so zyrisd does it. But `write`/`mkdir` take **paths that do
not exist yet** as valid targets (upstream `create_dir_all`s the parent), so a plain `canonicalize` fails
with `NotFound`. The rule:

> `canonicalize` up to the deepest ancestor that exists, then join the remaining components logically
> (resolving `..` at that point). If the result sits under one of `roots` and hits nothing in `deny`,
> let it through, and **pass the same canonical absolute path used for the check further down.**

`deny` wins over `roots`. Matching is an ancestor check on the canonical path (not globbing).

**`$HOME/.config/zyrisd/` is a hardcoded `deny` the user cannot remove.** This one is not security
theater — its `refresh_token` is all the refresh endpoint asks for, so that one file **re-issues the
node identity from anywhere, without this machine.** The "accident prevention" argument below applies
only to files whose blast radius is trapped on this machine.

#### exec is not delegated

Upstream `PtyTerminal::exec` spawns the child itself, reads to EOF with `cmd.output()`, and hands back
only the finished string — no pid, no `Child`, no stream escapes, and the timeout branch returns
immediately without killing. **So a decorator cannot kill the process group at all, and an output cap
only trims a string that is already fully in memory.**

`GatedTerminal::exec` is implemented directly instead of delegating (about 100 lines):

- spawn into a new process group with `Command::process_group(0)`
- read stdout/stderr only up to the cap; past it, stop reading and append a truncation marker
- on timeout: `killpg(SIGTERM)` → grace → `killpg(SIGKILL)` → `wait`
- effective timeout = `min(the caller's timeout_ms, exec_timeout_secs)`

Zombies do not pile up (tokio reaps on `Child` drop). What is left behind are **orphaned grandchildren
that keep running**, and that is what the process-group kill prevents.

The PTY calls do delegate. But the `PtyTerminal` instance is **built once and never rebuilt** —
the session cap is per instance and the reaper holds a `Weak`, so on drop every session that was open
disappears with it.

#### The inherited environment

The PTY shell inherits the daemon's whole environment, and a systemd **user** unit's environment is what
the session pushed in at graphical login. Measured here: `SSH_AUTH_SOCK=/run/user/1000/gcr/ssh`,
`DBUS_SESSION_BUS_ADDRESS` and `XAUTHORITY` are all in there. Omitting `Environment=` from the unit does
not fix it.

`[terminal] unset_env` defaults to `["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]`. Both **carry credentials to
other machines**, so their blast radius leaves this box. `DBUS_SESSION_BUS_ADDRESS` is machine-local and
needed for notifications, so it stays. To undo all of it: one line, `unset_env = []`.

#### What this gate is

**With `terminal` on offer, an agent can open any file through the shell anyway.** This gate is not
intrusion prevention, it is accident prevention. That is why `deny` defaults to empty — blocking
`~/.ssh` by default fails via `file_io` and succeeds via `cat`, an inconsistency that makes protection
that isn't there look real. The one exception is the hardcoded `deny` above; its blast radius differs.

**A gate cannot change the announced tool descriptions.** The macro's `descriptor()` ignores `T`,
fixed string, unrelated to `T`, and it says of every `file_io` method: "absolute paths are used as-is".
So the refusal **carries the allowed roots in its error message** and lets the model correct itself.

### 5.3 Lifecycle and exit codes

```
let caps = runner.capabilities();          // grab the handle before spawning
let slot = Arc::new(Mutex::new(None));     // filled synchronously in the on_connect closure

main ─┬─ tokio::spawn(runner.try_run())
      ├─ tokio::spawn(display_watch(caps, …))   // child probe, add/remove, retry
      └─ select! { try_run done      → map the exit code
                   SIGTERM | SIGINT  → close() the Connection in slot → 200ms
                                       → kill+reap the child → exit 0 }
```

`on_connect` is bound as `F: Fn(Connection) -> Fut`, and the runner spawns the future the hook returns.
**If the slot fill lives inside that future, a signal can land before it is scheduled**, so fill it
synchronously in the closure body and return `ready(())`.

Use `try_run()` rather than `Runner::run()` and let zyrisd map the exit code itself.
(`tokio::spawn(runner.try_run())` compiles — checked. Every field of `Runner` and the returned future are
`Send`, and the `Sync` that `serve_until_closed(&self)` demands holds too.)

**SIGTERM.** Upstream `Runner` waits only on `ctrl_c()` (=SIGINT), and only while a connection is alive.
systemd stops it with SIGTERM, so left alone the process dies without a close frame. A signal arriving
mid-dial or during backoff is caught by nobody, SIGINT included. `Connection` is `Clone` and
`close(&self, reason)` is sync, so no upstream patch is needed. The slot is not cleared on a drop, so
**check `is_closed()` before using it** (calling close again on a closed one is harmless).

On SIGINT both the zyrisd handler and the runner's `ctrl_c` fire (tokio broadcasts to every
listener), but both paths converge on exit 0, so it is safe.

**Exit codes.**

| Code | When | What the unit does |
|---|---|---|
| 0 | Clean exit on SIGTERM/SIGINT | No restart |
| 2 | `Credentials(NeedsOperator)` · `Build` · **`Refused` + credential-gave-up flag** · config syntax error | `RestartPreventExitStatus=2` |
| 1 | everything else (including `Refused` unrelated to credentials) | restart |

`RunError::Refused` **folds in every `WireError` with `retriable == false`**, so it is not only "the server
rejected this node". `ParseError`, `Internal` and `PayloadTooLarge` land here too, and one 500 mid-deploy
must not park the daemon forever. But `Refused` is a `String`, so no `ErrorCode` survives.

Instead of string matching, **our `Credentials` tells us.** The moment `rotate` gets `Ok(None)` and judges
this credential certainly dead, it sets an `AtomicBool`, and `main` reads it. If the flag is up,
`Refused` means revocation, so 2; otherwise 1.

`Build` mapping to 2 matches upstream — a wiring error like duplicate capability descriptor names, which a
restart will not fix.

Just before exiting 2, if `notify.enabled`, try a notification, best effort. **The body is a fixed string
zyrisd writes itself** (the server's string only goes to the log), and the call spells out the end of
options: `notify-send -- <summary> <body>`. The session environment is re-read every time, as in §6.

**Idle watch.** The node-side `Connection` at the pinned rev has no heartbeat timer, no silence timeout,
and the backoff loop only runs *after* a drop is detected. A half-open TCP left by laptop suspend or NAT
expiry goes unnoticed. If nothing arrives for `HelloAck.heartbeat` × 3 after the last message, zyrisd
calls `close()` to make the runner redial.

**Logging.** `main` installs the subscriber. With no `RUST_LOG`, `zyrisd=info,zyris=info`.
**Logs go to stderr** — the enrollment code block of `zyrisd enroll` is on stdout, and mixing the two
wrecks the one screen a human has to read. Under the unit, stderr goes to the journal.

Start from `RunConfig::from_env()`, overwrite the pub fields with config-file values, assemble the
credentials, then `Runner::new(config, credentials)` (`scopes_pinned` is private, so no struct update —
assign the fields one at a time).

## 6. Desktop helper

### Re-read the session environment every time

Display detection hangs entirely on environment variables (capkit `WAYLAND_DISPLAY`, xcap `DISPLAY`)
and **a process environment is frozen at exec.** With linger + `WantedBy=default.target` the daemon comes
up at boot, *before* graphical login, so neither variable is there, and logging in later does not apply
retroactively to a running process. Nailing `Environment=DISPLAY=:0` into the unit is the wrong answer.

**On every probe**, read `systemctl --user show-environment` (on failure, fall back to scanning
`/run/user/<uid>/wayland-*` and `/tmp/.X11-unix/X*`) and inject `DISPLAY`·`WAYLAND_DISPLAY`·
`XAUTHORITY`·`DBUS_SESSION_BUS_ADDRESS` into the child and into `notify-send`.

### Finding and watching the child

```
$ZYRISD_DISPLAY_BIN                 ← zyrisd install writes the absolute path into the unit
current_exe()/../libexec/zyrisd-display
current_exe()/../zyrisd-display     ← convenience for cargo run
/usr/libexec/zyrisd-display
```

**`PATH` is not a candidate.** `libexec` paths are on nobody's `PATH` anyway, and whoever can put that
name early in `PATH` gets code re-run for the daemon's whole lifetime.

The `display_watch` task owns it. If the probe enumerates one display or more,
`caps.add(ScreenCaptureServer(proxy))`; if input works, `InputServer` goes on too. When the child dies,
`caps.remove`, then retry with backoff (1s→60s) — a screen that shows up later still attaches. `Capabilities`
is `Clone`, so moving it into the task costs nothing.

**Use only `add`/`remove`; never hand `replace()` a new instance.** Those two clone the existing `Arc` and
rebuild, so `PtyTerminal` is never dropped.

Detection is not left to capkit's backend choice. With no `WAYLAND_DISPLAY`, `ScreenBackend::detect()`
**returns Xcap without probing anything**, and it panics on some compositors. The probe must be active, and
must run inside the child so a panic cannot kill the daemon — the second reason for splitting it out.

### Protocol

One frame at a time on the child's stdin/stdout. **Its stdout carries frames only; logs go to stderr.**

```
[u32 BE json_len][json][u32 BE blob_len][blob]
   json ≤ 1 MiB,  blob ≤ 8 MiB — anything larger: drop the frame and restart the child
```

Requests carry an `id`, one at a time. On timeout, kill + reap the child and start it again. **On EOF or a
partial frame, fail the in-flight request immediately** — otherwise a caller hangs until timeout when the
child dies mid-screenshot.

Messages:

| Request | Response |
|---|---|
| `Probe` | `{displays: [...], input_ok: bool}` |
| `ListDisplays` | `{displays: [...]}` — the probe cache is invalidated when the child restarts |
| `Screenshot{display, region, format, max_width}` | blob + `{resolved_display_id, sent_width, sent_height, media_type, description}` |
| `MoveTo`/`Click`/`Key`/`Type`/`Scroll` | ok |

Screenshot ships metadata because the capability returns not a raw blob but
`Datum::Image{name, description, media_type, blob}`, and **only the child can compute the
`description`** — after scaling to fit the budget, image coordinates drift from display coordinates,
and `description` is where that factor goes. The image is `Blob::Inline(Bytes)`, so no base64 bloat.

## 7. Configuration

`$HOME/.config/zyrisd/config.toml`. If it is absent, everything runs on defaults.

```toml
[node]
name       = "build-box-ruma"                  # default: <hostname>-<username>
server_url = "wss://attacca.cc/api/zyris/v1/ws"

[files]
roots = ["~"]        # roots[0] anchors relative paths and PTY cwd. The rest are an allowlist
deny  = []           # e.g. ["~/.ssh"].  $HOME/.config/zyrisd is always implicitly included

[terminal]
max_output_bytes  = 262144    # 256 KiB
exec_timeout_secs = 120       # effective = min(caller value, this value)
unset_env = ["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]

[desktop]
enabled = true

[notify]
enabled = true
```

Path rules — leaving nothing for the implementer to ask about:

- Expand only `~` and `~/` to `$HOME` (`~user` is unsupported). Neither TOML nor serde nor std expands
  `~`, so skipping this turns **the documented default itself into a "path that does not exist".**
- Relative paths are rejected (config error).
- `deny` wins over `roots`. Matching is an ancestor check on the canonical path.

Why `max_output_bytes` is not 1 MiB: Attacca measures a tool result with `serde_json::to_vec().len()`
and compares it to `ZYRIS_MAX_RESULT_BYTES` (default **1,000,000**). 1 MiB is already over budget before
JSON escaping and the envelope are added. This value is the stdout+stderr total; over it, output gets a
truncation marker and comes back **as success** (not an error).

Why the default node name is more than the hostname: two users on one machine put two identically named
nodes on Attacca, and nowhere in the install flow does anything notice.

**Config errors split in two.** Static errors — bad syntax, a relative path — exit 2. But a path in `roots`
that **does not exist is not exit 2** — if even one path comes up late (NFS, automount, removable disk,
LUKS), the daemon started at boot dies, systemd gives up restarting exactly as designed, and when the
mount appears ten seconds later a human has to start it by hand. A missing root is **warned about and
dropped from the allowlist** (narrower access, never wider). If every root is gone, exit 1 and retry.

## 8. Install and packaging

Enrollment approval is a human act, so **"works right after install" is impossible in principle.** Three steps.

```
① install binary   .deb or install.sh
② zyrisd enroll    show the code, a human approves
③ zyrisd install   write the unit + enable-linger + enable --now + confirm it is alive
```

**`install.sh` does ① only.** ② and ③ end as printed instructions. If the script did ③ too, the daemon
would come up unenrolled, die with exit 2, and `RestartPreventExitStatus=2` would freeze the unit failed —
and under `Type=simple`, `enable --now` returns exit 0 right after the fork, so **the script prints
"install complete" without ever knowing.**

After `start`, `zyrisd install` polls `is-active` and `ExecMainStatus` and **reports actual survival.**

### The systemd user unit

A system unit does not work. Three independent reasons reach the same conclusion.

- The PTY shell inherits the daemon's environment and defaults to `$SHELL`, which the system manager lacks.
- `/run/user/<uid>` is `0700`, so a service under a different uid cannot reach the user session.
- Credentials are a `0600` file and loading refuses group/other bits, so the uid has to match.

**The package does not ship the unit file; `zyrisd install` generates it.** It writes into
`~/.config/systemd/user/` and fills `ExecStart` with the absolute path from `current_exe()` (on Linux that
is `/proc/self/exe`, so symlinks resolve all the way). If the package owned the unit, `.deb` (`/usr/bin`)
and `install.sh` (`~/.local/bin`) would demand different `ExecStart` lines and drift apart.

```ini
[Unit]
Description=Zyris daemon
# If the unit outlives its ExecStart binary, systemd spins a restart loop on 203/EXEC.
ConditionPathExists=/absolute/path/zyrisd

[Service]
Type=simple
ExecStart=/absolute/path/zyrisd run
Environment=ZYRISD_DISPLAY_BIN=/absolute/path/zyrisd-display
RuntimeDirectory=zyrisd
Restart=on-failure
RestartSec=5
RestartPreventExitStatus=2
# Cut down from the default 90s. This grace is not for the daemon (200ms) but for the PTY children
# the agent left open, because expiry SIGKILLs the whole cgroup.
TimeoutStopSec=10

[Install]
WantedBy=default.target
```

`loginctl enable-linger` is **a required install step** (confirmed: an ordinary user can turn it on for
themselves without privilege escalation). Without linger the user manager only starts at login, so "starts
automatically at boot" does not hold.

`KillSignal=SIGTERM` is not written — it is the default and changes nothing.
`After=network-online.target` is out too. User units ignore it silently while
`systemd-analyze --user verify` passes it, so having it only fools you into thinking it does something.

Idempotent `zyrisd install` does not mean "do nothing". **If `ExecStart` or the binary version changed, it
restarts as well** — `enable --now` is a no-op on an already active unit, so otherwise the old binary keeps
running after an upgrade.

So `zyrisd status` can read it, the daemon atomically updates
`$XDG_RUNTIME_DIR/zyrisd/state.json` (= `RuntimeDirectory=zyrisd`) whenever the announced set changes. It
shifts at runtime as the child lives and dies, so the config alone cannot tell you.

### `zyrisd uninstall`

Spell out what gets removed: unit stop → disable → delete the file → `disable-linger` →
`$XDG_RUNTIME_DIR/zyrisd/`. With `--purge`, `$HOME/.config/zyrisd/` (credentials and config) as well.

`apt remove` cannot delete a per-user unit and linger it never created, so postrm
**tells you to run `zyrisd uninstall` first.** Remove the package first and `ConditionPathExists=`
quietly turns the unit inert.

### .deb

```
/usr/bin/zyrisd
/usr/libexec/zyrisd-display
```

Without a `maintainer-scripts` directory, cargo-deb writes **no postinst at all**
(this design skips `systemd-units`, so there is no auto-generated path). Write the postinst/postrm holding
those instructions into that directory by hand.

**Spell out `depends` by hand.** Left empty, cargo-deb runs `dpkg-shlibdeps` over **every** binary in the
package and lifts the child's libX11, libwayland and pipewire SONAMEs into `Depends:` —
which throws away the whole point of splitting the child out to keep headless installs graphics-free.
Write `depends = ["libc6 (>= …)"]` yourself and put the graphics libraries in `recommends`. The price is
maintaining the libc floor by hand, and that goes into §11.

### install.sh

POSIX sh. Fetch the tarball and `SHA256SUMS` from `$ZYRISD_BASE_URL` (default GitHub Releases), verify,
unpack into `~/.local/bin` and `~/.local/libexec`, then print the next steps **with the absolute path just
unpacked** (resolving by name could hit an old `/usr/bin/zyrisd`).

**The checksums come from the same base URL, so this catches transport corruption only.** State in the
script and the docs that it is no supply-chain guarantee. Signing is outside v1.

Do not decide whether `systemctl --user` works from the exit code of `is-system-running` — one unrelated
degraded unit misjudges a healthy machine (reproduced right here). Decide on the existence of
`XDG_RUNTIME_DIR` and on `systemctl --user show -p Version`.

## 9. Tests

The in-memory duplex in `zyris::testing` runs an announce round trip with no server and no DB (at this rev
it is not feature-gated). Gating, credentials and the desktop proxy are all tested in-process.

- **Gating** — absolute paths outside `roots` refused, symlink escapes refused, **write/mkdir to a path
  that does not exist yet passes**, `deny` wins, config cannot erase the hardcoded `deny`, `unset_env` applied.
- **exec** — output cap and truncation marker, effective timeout is the `min`, and after a timeout **no
  live grandchild process is left behind**.
- **Credentials** — empty store → `NeedsOperator`, expired → `force_refresh` rotation, refused → the store
  empties and the flag goes up, **a `Transport` error is `Unavailable` (retry)**, a permission error is 2.
- **Exit codes** — the whole table. Notably `Refused` + flag → 2, `Refused` alone → 1, `Build` → 2.
- **Desktop** — framing round trip, blob boundaries, over-length frames, **an in-flight request failing
  immediately on EOF**, `remove` when the child dies, same `PtyTerminal` instance after `add`/`remove`.
- **SIGTERM** — run `zyrisd run` as a child process against a minimal axum ws accept server, `kill -TERM`
  it, and check the close frame arrives (integration test).

Tests go in an inline `mod tests` at the bottom of each source file, names in snake_case, with a doc
comment above saying what it protects. Do not add a `rustfmt.toml`; match nearby line width by eye.

## 10. Non-goals

Serving connections to other Zyris programs, self-update, multi-profile, static `znt_` tokens, signing,
the `browser` capability. Windows/macOS get the `ServiceManager` trait only; impls are `unimplemented!`.

## 11. What stays unverified

The work machine is Arch Linux / GNOME Wayland / 3.6GB RAM. The following **can be built but not
here.**

| Item | Why | How it gets confirmed |
|---|---|---|
| real `dpkg -i` install | no dpkg here | Debian/Ubuntu container |
| **cargo-deb output** (`Depends`/`Recommends`/postinst) | cargo-deb not installed | install it, `cargo deb --no-build` → `ar x` and read control.tar |
| dash compatibility | `/bin/sh` is a bash symlink | `sh install.sh` in `debian:bookworm-slim` |
| glibc compat of the shipped binary · `libc6` floor | 2.44 here → 2.36 on bookworm | release build in an old-glibc container (CI) |
| desktop capture actually working | GNOME has no `zwlr_screencopy` | a sway/wlroots or X11 session |
| **display detection and notify on the boot (linger) path** | needs a reboot and graphical login | reboot for real, read the journal |
| detection on other desktops | one machine does not generalize | run `Probe` on KDE/sway/X11 |
| **`DT_NEEDED` of the parent binary being empty** | no artifact yet | `readelf -d target/release/zyrisd` |
| **release and checksum pipeline** | §12 policy forbids pushing | once CI exists |

Keep build parallelism at `-j2` or below. The desktop child builds zbus, pipewire bindgen and
wayland-scanner, so it is especially heavy.

## 12. Commit policy

This repo has **no remote.** Local commits for backup only. Nothing is pushed to GitHub.
So the GitHub Releases that §2 and §8 assume are **an address the installer points at, nothing more yet.**

## 13. Fix alongside once implementation starts

The repo map in `/home/ruma/CLAUDE.md` calls zyris-daemon "not a git repo" and records the zyris pin as
`branch = "main"`. This spec overturns both, so update it.
