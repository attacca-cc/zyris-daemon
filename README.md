# zyrisd

Keeps this machine connected to [Attacca](https://attacca.cc). Starts at boot, joins over the Zyris
protocol, and offers this machine to agents as capabilities.

| capability | when |
|---|---|
| `terminal` | Always. PTY shell and `exec` |
| `file_io` | Always. Read and write inside the configured roots |
| `screen_capture` | Only when a display is there that **actually captures** |
| `input` | Only when an input backend **actually opens** |

The last two are decided by **taking one small capture**, not by "a display looks present". Claiming
them when they don't work leaves the agent no way to tell "absent" from "broken".

## Install

```bash
curl -fsSL https://github.com/attacca-cc/zyris-daemon/releases/latest/download/install.sh | sh
```

Or grab the `.deb` and `sudo dpkg -i zyrisd_*.deb`.

Installing only drops the binary. **The last two steps are done by hand:**

```bash
zyrisd enroll     # prints an 8-character code. Approve it in the browser
zyrisd install    # makes it connect automatically on every boot
```

A human has to approve enrollment, so "works the moment it installs" is impossible. If the
installer also started the service, the daemon would come up unenrolled and sit there failing, and
with `Type=simple` the installer would print "done" without ever knowing.

## Windows

From PowerShell (native):

```powershell
irm https://github.com/attacca-cc/zyris-daemon/releases/latest/download/install.ps1 | iex
```

From Git Bash / MSYS2 / WSL2 (shell):

```bash
curl -fsSL https://github.com/attacca-cc/zyris-daemon/releases/latest/download/install.sh | sh
```

Or grab the installer from [releases](https://github.com/attacca-cc/zyris-daemon/releases) and run
`zyrisd-setup-x86_64.exe`. Auto-connect at boot is an installer option, and the installer only drops
the binary — enrollment is still done by hand afterwards, same as on Linux.

Windows does not offer screen capture or input (`screen_capture`/`input`) yet. The capabilities
it does offer are `terminal` and `file_io`.

## Commands

| command | what |
|---|---|
| `zyrisd enroll` | Enrolls this machine with an Attacca account |
| `zyrisd run` | Runs the daemon (this is what systemd calls) |
| `zyrisd install` | Registers a service so it starts at boot |
| `zyrisd status` | Shows enrollment, service, connection, and capability state |
| `zyrisd uninstall [--purge]` | Removes it. `--purge` takes the credentials too |

Don't run `enroll` or `install` as root. This is a user session service, and enrolling as root
leaves the credentials in `/root`, where the daemon won't find them.

## Configuration

`~/.config/zyrisd/config.toml`. Without it, everything runs on defaults.

```toml
[node]
name       = "build-box-ruma"                  # default: <hostname>-<username>
server_url = "wss://attacca.cc/api/zyris/v1/ws"

[files]
roots = ["~"]        # roots[0] anchors relative paths and the shell start dir
deny  = []           # e.g. ["~/.ssh"]

[terminal]
max_output_bytes  = 262144    # 256 KiB, stdout and stderr each
exec_timeout_secs = 120       # effective = min(caller's value, this)
unset_env = ["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]

[desktop]
enabled = true

[notify]
enabled = true
```

Paths expand only `~` and `~/`, and relative paths are rejected. A configured root that isn't up yet
(late NFS, automount, removable disk) drops out of the list instead of killing the daemon.

### What `roots` stops and what it doesn't

**As long as `terminal` is offered, the agent can open any file anyway through the shell.** `roots`
and `deny` are not intrusion prevention but **accident prevention** — they keep the agent from
wandering into the wrong directory. That's why `deny` defaults to empty. Denying `~/.ssh` by default
makes `file_io` fail where `cat` succeeds, and dresses up protection that isn't there.

One exception. `~/.config/zyrisd/` is denied in a way config cannot undo, because the refresh
token inside **reissues this node's identity without this machine**, past where that argument reaches.

The two `unset_env` defaults follow the same reasoning: `SSH_AUTH_SOCK` and `GPG_AGENT_INFO` carry
credentials to other machines, so the blast radius leaves this box. `unset_env = []` undoes it.

## When credentials are revoked

The daemon **does not re-enroll itself.** It raises a desktop notification, exits with code 2, and
systemd gives up restarting (`RestartPreventExitStatus=2`). Otherwise enrollment codes pile up over
and over in a journal nobody reads.

To use it again, run `zyrisd enroll`. A successful enrollment revives the stopped service with it.

## Building from source

**There are two workspaces, and that is the core structure of this repo.**

| what | where | links graphics |
|---|---|---|
| `zyrisd`, `zyrisd-node`, `zyrisd-display-proto` | root workspace | ✗ |
| `zyrisd-display` | `display/` — its own workspace root | ✓ |

Cargo unifies features not per crate but across **every workspace member pulled into a single
cargo invocation**. Put both in one workspace and one `cargo build --workspace` nails the graphics
stack into the parent binary as `DT_NEEDED` too, and on a headless box without those `.so` files it
dies before `main` runs. Splitting the workspaces makes that accident structurally impossible.

```bash
cargo build -p zyrisd --release                            # parent
cargo build --manifest-path display/Cargo.toml --release   # desktop helper
./scripts/check-zyris-pin.sh                               # is the zyris dep pinned to one rev
```

Never use `--workspace`. Check that the split held with this:

```bash
readelf -d target/release/zyrisd | grep -Ei 'X11|wayland|xcb|gbm'   # must print nothing
```

Building the desktop helper needs the X11/Wayland dev packages. Building only the parent does not.

### Packaging

```bash
cargo deb -p zyrisd --no-build     # target/debian/zyrisd_*.deb
```

The `.deb` depends only on `libc6`; graphics libraries sit under `Recommends:`. The helper is a
separate process precisely so headless installs don't drag in the graphics stack, so promoting it
to `Depends:` here makes that split pointless.

GitHub Actions does the release builds. Push a `v*` tag and `.github/workflows/release.yml` builds
Linux (x86_64, aarch64) and Windows (x86_64), then uploads `install.sh`, `install.ps1`, tarball, `.deb`,
and the `.exe` installer to GitHub Releases.

## License

Your choice of [MIT](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE).

Contributions are dual-licensed under both of the above unless you say otherwise.
