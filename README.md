# dsh-superagent-launcher

A small native window (Rust + [egui](https://github.com/emilk/egui)) that finds every
[DeepSeek Harness](https://github.com/deepseek-ai) profile mounting the `dsh-superagent`
plugin and lets you start / open / stop / delete one of them — or add a repository on a
fresh profile and port. No auto-start: nothing here runs at login.

```
┌────────────────────────────────────────────────────────────────────┐
│ superagent-lantern   :3080   running                                │
│   repo  Me106y/agent-lantern                                        │
│   cli   claude          allow  Me106y,octocat                       │
│                     [Open]  [Stop]  [Configure]  [Delete…]          │
└────────────────────────────────────────────────────────────────────┘
```

## What it reads

| Fact | Where it comes from |
| --- | --- |
| which profiles matter | `<dsh home>/profiles/*/package.json` containing `dsh-superagent` |
| the port | `cordis.patch.yml`, `- id: webserver` → `port` |
| the state directory | `cordis.patch.yml`, `- id: dsh-superagent` → `stateDir` |
| the repository and agent CLI | `<stateDir>/orchestrator.sqlite`, newest `settings-version:*` row with `block = "source:github"` |
| the allowed logins | the same table, `block = "policy"` |
| running / stopped | a TCP connect to `127.0.0.1:<port>` |

The repository is **panel state, not YAML**, which is why a stopped instance still shows
its repository. The database is read through `/usr/bin/sqlite3` rather than a linked
SQLite client: the plugin keeps it in WAL mode, so a `-readonly` connection has no `-shm`
to attach to once the process is gone, while a normal connection with a busy timeout reads
safely either way.

## What it does

- **Start** — spawns `node <runtime>/node_modules/@deepseek-ai/dsh/lib/bin.js --profile <p> --no-open`
  detached, with output going to `~/Library/Logs/dsh-superagent-<p>.log`.
- **Open** — reads the login token the launcher printed into that log and opens
  `http://127.0.0.1:<port>/?token=<token>`.
- **Stop** — `SIGTERM`, never `SIGKILL`: the plugin releases its state-directory lock on a
  clean exit. The process is found by its command line (`--profile <name>`), so a port
  reused by something else is never signalled by mistake.
- **Delete** — stops the instance, waits for the process to actually exit, then removes the
  profile directory and its log. The state directory (ticket database, per-ticket
  checkouts) is the only part that cannot be rebuilt, so it is **kept** unless the
  confirmation's "also delete the state directory" box is ticked; even then it is only
  removed when it is a plain directory under `<dsh home>/state` — never a symlink, never a
  path outside it. If the process cannot be confirmed stopped, nothing is deleted.
- **Add repository** — copies a template profile (`cp -a`), rewrites its port and state
  directory, starts it, then binds the repository through the plugin's own
  `PUT /orchestrator/api/settings/source:github`. Going through the API is deliberate:
  it runs the plugin's setup check (repository readable, agent account can push) and
  restarts it. Editing files directly would skip that check. The agent CLI
  (`<cli> api user --jq .login`) is run first, so a broken wrapper fails before a profile
  exists; if a later step fails, the profile this job created is removed again. A profile
  left over from an earlier failed attempt (`not configured`) is restarted and reused
  instead of refused, so a retry cannot inherit the old window's PATH.

A window opened from Finder inherits launchd's PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), which
has neither Homebrew nor a per-user bin directory. Agent CLIs are usually shell wrappers that
call `gh` by name, so every spawned profile gets `/opt/homebrew/bin`, `/usr/local/bin`,
`~/…/sbin`, `~/.local/bin` and `~/.npm-global/bin` added to its PATH; without that the plugin's
own setup check fails with `gh: not found`.

## Build and run

```sh
cargo build --release          # needs Rust 1.85+ (edition 2024)
./target/release/dsh-superagent-launcher
./scripts/install-app.sh       # refresh ~/Applications/DSH Superagent.app
```

The app bundle in `~/Applications` is what Finder and Spotlight launch, and it carries its own
copy of the binary: `cargo build` alone never updates it. `scripts/install-app.sh` rebuilds,
replaces that copy and re-signs the bundle ad-hoc (a replaced executable invalidates the
signature, and macOS refuses to start it).

Headless modes, used by the smoke tests:

```sh
dsh-superagent-launcher --list                       # tab-separated view of the same rows
dsh-superagent-launcher --add owner/name [--allow login,...] [--cli path]
dsh-superagent-launcher --accounts <port> <logins> <cli>
dsh-superagent-launcher --remove <profile> [--purge]
```

## Requirements

- macOS, a DSH installation under `~/Library/Application Support/DeepSeekHarness/runtime`,
  `node`, `curl` and `sqlite3` on the system paths this binary uses.
- `DSH_HOME` is honoured; otherwise `~/.dsh` is assumed.

## Platform status

**macOS only today.** Nothing here uses a macOS-only API — it is a plain eframe binary —
but the install directory, the log directory, and the `open` / `ps` / `sqlite3` invocations
are hard-coded to macOS paths. A Linux port is a small `platform` module plus a runtime
probe for the DSH install directory; the profile/port/SQLite/settings-API logic is already
portable. The UI text is English on purpose: egui ships no CJK font and bundling a system
font is not something this tool should do.

## License

No license is granted yet. Ask before reusing.
