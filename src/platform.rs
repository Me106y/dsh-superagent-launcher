//! Platform facts: where DeepSeek Harness is installed, where logs go, how URLs open, and how the plugin's
//! SQLite database is read.
//!
//! macOS keeps the runtime under `~/Library/Application Support/DeepSeekHarness/runtime` and opens URLs with
//! `open`. On Linux the `@deepseek-ai/dsh` package lives in a Node installation that the launcher has to find
//! itself, because a desktop-launched process does not inherit the shell's `PATH`; URLs open with `xdg-open`.
//! SQLite is linked instead of shelled out to `/usr/bin/sqlite3`, which many Linux distributions do not ship.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Where `start` writes the console log it later reads the login token from.
pub fn logs_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home().join("Library/Logs")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/state"))
            .join("dsh-superagent")
    }
}

fn executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Every `node_modules` directory a global install could sit in, newest convention first.
fn node_roots() -> Vec<PathBuf> {
    let home = home();
    let mut roots = vec![
        PathBuf::from("/usr/local/lib/node_modules"),
        PathBuf::from("/usr/lib/node_modules"),
        home.join(".local/lib/node_modules"),
    ];
    if let Some(dir) = std::env::var_os("DSH_INSTALL_DIR") {
        roots.insert(0, PathBuf::from(dir).join("node_modules"));
    }
    // Per-user installs: `~/local/node-v22…`, `~/.nvm/versions/node/v22…`.
    for parent in [home.join("local"), home.join(".nvm/versions/node")] {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            roots.push(entry.path().join("lib/node_modules"));
        }
    }
    roots
}

/// The JavaScript entry point of the DSH application: `node <entry> --profile <name> --no-open`.
pub fn dsh_entry() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let installed = home().join(
            "Library/Application Support/DeepSeekHarness/runtime/node_modules/@deepseek-ai/dsh/lib/bin.js",
        );
        if installed.exists() {
            return Some(installed);
        }
    }
    let relative = Path::new("@deepseek-ai/dsh/lib/bin.js");
    for root in node_roots() {
        let candidate = root.join(relative);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Last resort: the `dsh` on `PATH` is usually a symlink straight at `lib/bin.js`.
    let output = Command::new("sh")
        .args(["-c", "command -v dsh"])
        .output()
        .ok()?;
    let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!found.is_empty()).then(|| fs::canonicalize(&found).unwrap_or_else(|_| PathBuf::from(found)))
}

/// The `node` binary that runs the DSH entry point.
pub fn node_bin() -> PathBuf {
    if let Some(value) = std::env::var_os("NODE_BIN") {
        return PathBuf::from(value);
    }
    let home = home();
    let mut candidates = vec![
        PathBuf::from("/usr/local/bin/node"),
        PathBuf::from("/usr/bin/node"),
        home.join(".local/bin/node"),
    ];
    for parent in [home.join("local"), home.join(".nvm/versions/node")] {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                candidates.push(entry.path().join("bin/node"));
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':').filter(|dir| !dir.is_empty()) {
            candidates.push(PathBuf::from(dir).join("node"));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| executable(candidate))
        .unwrap_or_else(|| PathBuf::from("node"))
}

/// The PATH a spawned profile should see.
///
/// A desktop launch does not inherit the shell's PATH: macOS hands the app
/// `/usr/bin:/bin:/usr/sbin:/sbin`, and a desktop entry on Linux is no better. The agent CLI is usually a
/// shell wrapper around `gh`, which lives in Homebrew or `/usr/local/bin`, so every child that runs the CLI
/// — the profile itself and the preflight checks — needs those directories on its PATH.
pub fn child_path() -> String {
    let mut entries: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();
    let home = home();
    let mut extra = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/sbin"),
        PathBuf::from("/usr/local/sbin"),
        home.join(".local/bin"),
        home.join(".npm-global/bin"),
    ];
    for parent in [home.join("local"), home.join(".nvm/versions/node")] {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                extra.push(entry.path().join("bin"));
            }
        }
    }
    for path in extra {
        let path = path.display().to_string();
        if !entries.iter().any(|entry| entry == &path) {
            entries.push(path);
        }
    }
    entries.join(":")
}

/// Resolve a configured CLI to a runnable path, so a bare `gh-agent` also works from a desktop launch.
pub fn resolve_program(program: &str) -> PathBuf {
    if program.contains('/') {
        return PathBuf::from(program);
    }
    for dir in child_path().split(':') {
        let candidate = Path::new(dir).join(program);
        if executable(&candidate) {
            return candidate;
        }
    }
    PathBuf::from(program)
}

/// Open a URL in the user's browser.
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "/usr/bin/open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let _ = Command::new(opener).arg(url).spawn();
}

/// Every `settings-version:*` value in the plugin's key-value table.
///
/// A normal connection with a busy timeout: the plugin keeps the database in WAL mode, and a read-only
/// connection has no `-shm` to attach to once the process is gone.
pub fn kv_values(db: &Path) -> Vec<String> {
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(connection) = rusqlite::Connection::open_with_flags(db, flags) else {
        return Vec::new();
    };
    let _ = connection.busy_timeout(Duration::from_millis(1500));
    let Ok(mut statement) =
        connection.prepare("select value from kv where key like 'settings-version:%'")
    else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.filter_map(Result::ok).collect()
}
