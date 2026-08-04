# zyrisd

Keeps this machine connected to [Attacca](https://attacca.cc). Starts at boot, joins over the Zyris
protocol, and offers this machine's terminal and files — plus screen capture and input, on a
desktop where those actually work — as capabilities agents can use.

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
installer also started the service, the daemon would come up unenrolled and sit there failing.

## Commands

| command | what |
|---|---|
| `zyrisd enroll` | Enrolls this machine with an Attacca account |
| `zyrisd run` | Runs the daemon (this is what systemd calls) |
| `zyrisd install` | Registers a service so it starts at boot |
| `zyrisd status` | Shows enrollment, service, and capability state |
| `zyrisd uninstall [--purge]` | Removes it. `--purge` takes the credentials too |

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
max_output_bytes  = 262144    # 256 KiB
exec_timeout_secs = 120       # effective = min(caller's value, this)
unset_env = ["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]

[desktop]
enabled = true

[notify]
enabled = true
```

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
systemd gives up restarting. Otherwise enrollment codes pile up in a journal nobody reads.

To use it again, run `zyrisd enroll`. A successful enrollment revives the stopped service with it.

## License

MIT or Apache-2.0.
