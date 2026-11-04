# dsh-superagent-launcher

A small native window (Rust + [egui](https://github.com/emilk/egui)) that finds every
[DeepSeek Harness](https://github.com/deepseek-ai) profile mounting the `dsh-superagent`
plugin and lets you start / open / stop one of them — or add a repository on a fresh
profile and port. No auto-start: nothing here runs at login.

```
┌──────────────────────────────────────────────────────────────┐
│ superagent-lantern   :3080   running                          │
│   repo  Me106y/agent-lantern                                  │
│   cli   claude          allow  Me106y,octocat                 │
│                              [Open]  [Stop]                   │
└──────────────────────────────────────────────────────────────┘
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
  clean exit. The port is only a hint — the process command line must actually name this
  profile, so a port reused by something else is never signalled by mistake.
- **Add repository** — copies a template profile (`cp -a`), rewrites its port and state
  directory, starts it, then binds the repository through the plugin's own
  `PUT /orchestrator/api/settings/source:github`. Going through the API is deliberate:
  it runs the plugin's setup check (repository readable, agent account can push) and
  restarts it. Editing files directly would skip that check.

## Build and run

```sh
cargo build --release          # needs Rust 1.85+ (edition 2024)
./target/release/dsh-superagent-launcher
```

Headless modes, used by the smoke tests:

```sh
dsh-superagent-launcher --list                       # tab-separated view of the same rows
dsh-superagent-launcher --add owner/name [--allow login,...]
dsh-superagent-launcher --accounts <port> <logins> <cli>
```

## Requirements

- macOS, a DSH installation under `~/Library/Application Support/DeepSeekHarness/runtime`,
  `node`, `curl`, `sqlite3` and `lsof` on the system paths this binary uses.
- `DSH_HOME` is honoured; otherwise `~/.dsh` is assumed.

## Platform status

**macOS only today.** Nothing here uses a macOS-only API — it is a plain eframe binary —
but the install directory, the log directory, and the `open` / `lsof` / `sqlite3` invocations
are hard-coded to macOS paths. A Linux port is a small `platform` module plus a runtime
probe for the DSH install directory; the profile/port/SQLite/settings-API logic is already
portable. The UI text is English on purpose: egui ships no CJK font and bundling a system
font is not something this tool should do.

## License

No license is granted yet. Ask before reusing.
