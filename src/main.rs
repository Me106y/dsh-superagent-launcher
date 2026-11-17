//! DSH Superagent launcher.
//!
//! Finds every DSH profile that mounts `dsh-superagent`, shows the repository it is bound to and the port it
//! serves, and lets you start / open / stop one of them, add a new repository on a fresh profile and port, or
//! delete a profile that is no longer wanted. No auto-start: nothing here runs at login.
//!
//! Facts that drive the design:
//!
//! * The port lives in `<profile>/cordis.patch.yml` under the `webserver` row.
//! * The repository does **not** live in YAML. It is panel state, stored in `<stateDir>/orchestrator.sqlite`
//!   under the newest `settings-version:N` key. Reading that table is what lets a stopped instance still show
//!   its repository, so this binary shells out to `/usr/bin/sqlite3` instead of linking SQLite.
//!   `-readonly` cannot be used: the plugin keeps the database in WAL mode, and a read-only connection has no
//!   `-shm` to attach to once the process is gone. A normal connection with a busy timeout reads safely.
//! * Binding a repository is a plugin operation, not a file edit: `PUT /orchestrator/api/settings/source:github`
//!   runs the plugin's own setup check (repository readable, agent account can push) and restarts it. Adding a
//!   repository any other way would skip that check.
//! * Deleting a profile is the opposite: the plugin has no delete endpoint, so it is a filesystem operation. The
//!   process is stopped and *waited for* before any file is touched, the state directory is kept unless asked
//!   for, and it is only removed when it is a plain directory under the DSH state root — never a symlink, never
//!   a path outside it.
//! * A window opened from Finder inherits launchd's PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), which has neither
//!   Homebrew nor a per-user bin directory. Agent CLIs are shell wrappers around `gh`, so every profile is
//!   spawned with a PATH that can find them; without it the plugin's own setup check reports `gh: not found`.
//!
//! UI text is English on purpose: egui ships no CJK font, and bundling a system font is not something this
//! tool should do (redistribution). Labels stay short and technical.

use eframe::egui;
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

/// Destructive actions share one colour in the UI.
const DANGER: egui::Color32 = egui::Color32::from_rgb(220, 90, 80);

/// A profile that mounts dsh-superagent.
#[derive(Clone)]
struct Instance {
    profile: String,
    port: u16,
    state_dir: PathBuf,
    repo: String,
    cli: String,
    allowed: Vec<String>,
    running: bool,
}

/// Progress of the background "add a repository" job.
enum Msg {
    Step(String),
    Done(Result<String, String>),
}

/// A delete that has been requested and is waiting for confirmation inside the instance card.
struct Deletion {
    profile: String,
    purge_state: bool,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn dsh_home() -> PathBuf {
    std::env::var("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".dsh"))
}

fn profiles_dir() -> PathBuf {
    dsh_home().join("profiles")
}

fn logs_dir() -> PathBuf {
    home().join("Library/Logs")
}

fn install_dir() -> PathBuf {
    home().join("Library/Application Support/DeepSeekHarness/runtime")
}

fn dsh_bin() -> PathBuf {
    install_dir().join("node_modules/@deepseek-ai/dsh/lib/bin.js")
}

fn node_bin() -> PathBuf {
    for candidate in ["/usr/local/bin/node", "/opt/homebrew/bin/node"] {
        if Path::new(candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("node")
}

/// The PATH a spawned profile should see.
///
/// Launchd gives a Finder-launched window only `/usr/bin:/bin:/usr/sbin:/sbin`. The agent CLI is typically a
/// shell wrapper that calls `gh` by name, so the profile needs the Homebrew and per-user bin directories even
/// when this binary itself was started with a minimal environment.
fn child_path() -> String {
    let mut entries: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();
    let mut extra: Vec<PathBuf> = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/homebrew/sbin", "/usr/local/sbin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        extra.push(Path::new(&home).join(".local/bin"));
        extra.push(Path::new(&home).join(".npm-global/bin"));
    }
    for path in extra {
        let path = path.display().to_string();
        if !entries.iter().any(|entry| entry == &path) {
            entries.push(path);
        }
    }
    entries.join(":")
}

fn log_for(profile: &str) -> PathBuf {
    logs_dir().join(format!("dsh-superagent-{profile}.log"))
}

/// Read one scalar out of a `- id: <id>` block in a cordis patch file.
///
/// A tolerant line scanner rather than a YAML parser: these patches may contain `!!js` tags that a strict
/// parser rejects, and the only values wanted are flat scalars.
fn block_scalar(text: &str, id: &str, key: &str) -> Option<String> {
    let mut current: Option<String> = None;
    let mut found = None;
    let prefix = format!("{key}:");
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- id:") {
            current = Some(rest.trim().trim_matches(|c| c == '"' || c == '\'').to_string());
            continue;
        }
        if current.as_deref() == Some(id) {
            if let Some(rest) = trimmed.strip_prefix(&prefix) {
                found = Some(
                    rest.trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_string(),
                );
            }
        }
    }
    found
}

/// Rewrite one scalar inside a `- id: <id>` block, keeping the line's indentation.
fn set_block_scalar(text: &str, id: &str, key: &str, value: &str) -> Option<String> {
    let mut current = String::new();
    let mut out = String::with_capacity(text.len() + 32);
    let mut replaced = false;
    let prefix = format!("{key}:");
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- id:") {
            current = rest.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        }
        if current == id && !replaced && trimmed.starts_with(&prefix) {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(&format!("{indent}{key}: {value}\n"));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    replaced.then_some(out)
}

/// The newest `settings-version:N` blob holds `settings.sources.github.config.{repository,cli}`.
fn settings_from_db(state_dir: &Path) -> Option<(String, String)> {
    let db = state_dir.join("orchestrator.sqlite");
    if !db.exists() {
        return None;
    }
    // One row per saved block: `source:github` holds the repository, `policy` holds the allowed list.
    // Reading only the newest row would break as soon as policy is saved after the source.
    let output = Command::new("/usr/bin/sqlite3")
        .args([
            "-cmd",
            ".timeout 1500",
            db.to_str()?,
            "select value from kv where key like 'settings-version:%';",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if parsed["block"] != "source:github" {
            continue;
        }
        let config = &parsed["settings"]["sources"]["github"]["config"];
        let repo = config["repository"].as_str().unwrap_or_default().to_string();
        let cli = config["cli"].as_str().unwrap_or_default().to_string();
        if !repo.is_empty() {
            return Some((repo, cli));
        }
    }
    None
}

/// Policy stores "who may ask for work" in its own row: `settings.policy.sources.github.allowed`.
fn allowed_from_db(state_dir: &Path) -> Vec<String> {
    let db = state_dir.join("orchestrator.sqlite");
    let Some(db) = db.to_str() else {
        return Vec::new();
    };
    let Ok(output) = Command::new("/usr/bin/sqlite3")
        .args([
            "-cmd",
            ".timeout 1500",
            db,
            "select value from kv where key like 'settings-version:%';",
        ])
        .output()
    else {
        return Vec::new();
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if parsed["block"] != "policy" {
            continue;
        }
        let Some(entries) = parsed["settings"]["policy"]["sources"]["github"]["allowed"].as_array()
        else {
            continue;
        };
        return entries
            .iter()
            .filter_map(|entry| entry["id"].as_str().map(str::to_string))
            .collect();
    }
    Vec::new()
}

fn is_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

fn discover() -> Vec<Instance> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(profiles_dir()) else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = fs::read_to_string(dir.join("package.json")).unwrap_or_default();
        if !manifest.contains("dsh-superagent") {
            continue;
        }
        let patch = fs::read_to_string(dir.join("cordis.patch.yml")).unwrap_or_default();
        let port = block_scalar(&patch, "webserver", "port")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(3080);
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let state_dir = block_scalar(&patch, "dsh-superagent", "stateDir")
            .map(PathBuf::from)
            .unwrap_or_else(|| dsh_home().join("state").join(&name));
        let (repo, cli) = settings_from_db(&state_dir).unwrap_or_else(|| ("not configured".into(), String::new()));
        let allowed = allowed_from_db(&state_dir);
        found.push(Instance {
            profile: name,
            port,
            state_dir,
            repo,
            cli,
            allowed,
            running: is_listening(port),
        });
    }
    found.sort_by_key(|instance| instance.port);
    found
}

/// Start one profile detached, writing its console output to a log this binary reads the token from.
fn start(profile: &str) -> Result<(), String> {
    let log = log_for(profile);
    if let Some(parent) = log.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let out = File::create(&log).map_err(|error| format!("cannot write {}: {error}", log.display()))?;
    let err = out.try_clone().map_err(|error| error.to_string())?;
    Command::new(node_bin())
        .arg(dsh_bin())
        .arg("--profile")
        .arg(profile)
        .arg("--no-open")
        .env("PATH", child_path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot start: {error}"))
}

/// The login token is printed once per launch; the last one in the log belongs to the running process.
fn last_token(profile: &str) -> Option<String> {
    let candidates = [
        log_for(profile),
        logs_dir().join("dsh-superagent.log"),
        dsh_home().join("state").join(profile).join("launch.log"),
    ];
    for path in candidates {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(index) = text.rfind("token=") else {
            continue;
        };
        let token: String = text[index + "token=".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !token.is_empty() {
            return Some(token);
        }
    }
    None
}

fn open_panel(profile: &str, port: u16) {
    let url = match last_token(profile) {
        Some(token) => format!("http://127.0.0.1:{port}/?token={token}"),
        None => format!("http://127.0.0.1:{port}/"),
    };
    let _ = Command::new("/usr/bin/open").arg(url).spawn();
}

/// Whether a command line is a DSH process at all, judged by its executable and the entry point it runs.
///
/// A command line that merely *mentions* `--profile <name>` is not this instance: a `bash -c` script, an
/// editor, or a log tail can contain that text, and signalling those would be a bug. Only a `dsh`
/// executable, or a `node` running a `dsh` entry point, counts.
fn is_dsh_command(command: &str) -> bool {
    let mut tokens = command.split_whitespace();
    let Some(executable) = tokens.next() else {
        return false;
    };
    let name = executable.rsplit('/').next().unwrap_or(executable);
    if name == "dsh" {
        return true;
    }
    name == "node"
        && tokens.any(|token| {
            token.ends_with("/dsh")
                || token.ends_with("/dsh/lib/bin.js")
                || token.contains("/@deepseek-ai/dsh/")
        })
}

/// Pids whose command line names this profile as `--profile <name>`.
///
/// Matching the command line rather than the listening port is what keeps a port reused by an unrelated
/// process out of reach.
fn profile_pids(profile: &str) -> Vec<String> {
    let Ok(output) = Command::new("/bin/ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };
    let flag = format!("--profile={profile}");
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.trim_start().splitn(2, char::is_whitespace);
        let (Some(pid), Some(command)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !is_dsh_command(command) {
            continue;
        }
        let tokens: Vec<&str> = command.split_whitespace().collect();
        let names_profile = tokens.iter().any(|token| *token == flag)
            || tokens
                .windows(2)
                .any(|pair| pair[0] == "--profile" && pair[1] == profile);
        if names_profile {
            pids.push(pid.to_string());
        }
    }
    pids
}

/// SIGTERM, never SIGKILL: the plugin releases its state-directory lock on a clean exit.
///
/// The port is only a hint. Before signalling, the process command line must actually name this profile,
/// so a port reused by something else is never killed by mistake. Returns without waiting for the exit;
/// callers that are about to touch files use [`wait_stopped`].
fn stop(profile: &str, port: u16) -> Result<(), String> {
    let pids = profile_pids(profile);
    if pids.is_empty() {
        return if is_listening(port) {
            Err(format!(
                "port {port} is in use but no process names --profile {profile}; stop it by hand and retry"
            ))
        } else {
            Ok(())
        };
    }
    for pid in &pids {
        let _ = Command::new("/bin/kill").args(["-TERM", pid]).status();
    }
    Ok(())
}

fn wait_stopped(profile: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if profile_pids(profile).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Call the plugin's management API. The header is the same-origin marker the plugin requires.
fn api_call(port: u16, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let url = format!("http://127.0.0.1:{port}/orchestrator/api/{path}");
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "-sS",
        "--max-time",
        "30",
        "-X",
        method,
        "-H",
        "x-orchestrator-request: 1",
        "-H",
        "content-type: application/json",
    ]);
    if let Some(payload) = body {
        command.args(["--data-binary", payload]);
    }
    command.args(["-w", "\n__STATUS__%{http_code}", &url]);
    let output = command.output().map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let (rest, status) = text.rsplit_once("__STATUS__").ok_or("unexpected curl output")?;
    let code = status.trim().parse::<u16>().map_err(|_| "unexpected status".to_string())?;
    Ok((code, rest.trim_end().to_string()))
}

fn wait_ready(port: u16, profile: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_listening(port) && last_token(profile).is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && repo.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/'
        })
}

fn slug_for(repo: &str) -> String {
    let tail: String = repo
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("superagent-{tail}")
}

fn next_free_port(instances: &[Instance]) -> u16 {
    let used: Vec<u16> = instances.iter().map(|instance| instance.port).collect();
    (3082..3200)
        .find(|port| !used.contains(port) && !is_listening(*port))
        .unwrap_or(3082)
}

/// Copy an existing profile, then point it at a new port and a new state directory.
fn create_profile(template: &str, repo: &str, progress: &dyn Fn(&str)) -> Result<(String, u16), String> {
    let name = slug_for(repo);
    let source = profiles_dir().join(template);
    let target = profiles_dir().join(&name);
    if !source.is_dir() {
        return Err(format!("template profile {template} does not exist"));
    }
    if target.exists() {
        return Err(format!(
            "profile {name} already exists; delete it first, or add a different repository"
        ));
    }

    progress("allocating a free port");
    let port = next_free_port(&discover());

    progress("copying the profile");
    let copied = Command::new("/bin/cp")
        .args(["-a"])
        .arg(&source)
        .arg(&target)
        .status()
        .map_err(|error| error.to_string())?;
    if !copied.success() {
        return Err("cp -a failed".into());
    }

    progress("writing port and state directory");
    let manifest_path = target.join("package.json");
    let manifest = fs::read_to_string(&manifest_path).map_err(|error| error.to_string())?;
    let mut manifest_json: Value = serde_json::from_str(&manifest).map_err(|error| error.to_string())?;
    manifest_json["name"] = json!(format!("dsh-profile-{name}"));
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest_json).unwrap_or(manifest))
        .map_err(|error| error.to_string())?;

    let patch_path = target.join("cordis.patch.yml");
    let patch = fs::read_to_string(&patch_path).map_err(|error| error.to_string())?;
    let state_dir = dsh_home().join("state").join(&name);
    let patch = set_block_scalar(&patch, "webserver", "port", &port.to_string())
        .ok_or("the template has no webserver port row")?;
    let patch = set_block_scalar(&patch, "dsh-superagent", "stateDir", &state_dir.display().to_string())
        .ok_or("the template has no dsh-superagent stateDir row")?;
    fs::write(&patch_path, patch).map_err(|error| error.to_string())?;
    fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;

    Ok((name, port))
}

/// Remove a path without ever following a symlink: a profile that links a checkout into its `node_modules`
/// must lose the link, not the checkout.
fn remove_tree(path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    let result = if meta.file_type().is_symlink() || !meta.is_dir() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    };
    result.map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

/// True when `child` is a real descendant of `parent`, after resolving symlinks on both sides.
fn is_under(child: &Path, parent: &Path) -> bool {
    let resolve = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (child, parent) = (resolve(child), resolve(parent));
    child != parent && child.starts_with(&parent)
}

fn valid_profile_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// Delete one configured instance: stop it, remove the profile and its log, and its state when asked.
///
/// The order matters. The process has to be gone before the profile directory is touched, otherwise the
/// plugin keeps writing into a profile that no longer exists. The state directory is the only part that
/// cannot be rebuilt, so it survives unless `purge_state` is set, and even then only when it is a plain
/// directory under the DSH state root — a state dir pointed somewhere else is reported, never removed.
fn remove_instance(profile: &str, purge_state: bool, progress: &dyn Fn(&str)) -> Result<String, String> {
    if !valid_profile_name(profile) {
        return Err(format!("not a profile name: {profile}"));
    }
    let Some(instance) = discover()
        .into_iter()
        .find(|instance| instance.profile == profile)
    else {
        return Err(format!("no profile named {profile} mounts dsh-superagent"));
    };
    let dir = profiles_dir().join(profile);
    let manifest = fs::read_to_string(dir.join("package.json")).unwrap_or_default();
    if !manifest.contains("dsh-superagent") {
        return Err(format!(
            "{} does not look like a dsh-superagent profile",
            dir.display()
        ));
    }

    progress("stopping it (SIGTERM)");
    stop(profile, instance.port)?;
    if !wait_stopped(profile, Duration::from_secs(15)) {
        return Err(format!("{profile} is still running; nothing was deleted"));
    }

    progress("removing the profile");
    remove_tree(&dir)?;

    let mut state = format!("state kept at {}", instance.state_dir.display());
    if instance.state_dir.starts_with(&dir) {
        state = "state removed with the profile".into();
    } else if purge_state {
        let is_symlink = fs::symlink_metadata(&instance.state_dir)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink || !is_under(&instance.state_dir, &dsh_home().join("state")) {
            state = format!(
                "state left at {} (not a plain directory under the DSH state root)",
                instance.state_dir.display()
            );
        } else {
            progress("removing the state directory");
            remove_tree(&instance.state_dir)?;
            state = "state removed".into();
        }
    }

    let log = log_for(profile);
    if log.exists() {
        let _ = fs::remove_file(&log);
    }
    Ok(format!("{profile} deleted — {state}"))
}

/// Bind the repository through the plugin's own settings API, which runs the setup check and restarts it.
fn bind_repository(port: u16, repo: &str, cli: &str, progress: &dyn Fn(&str)) -> Result<(), String> {
    progress("reading current settings");
    let (status, body) = api_call(port, "GET", "settings", None)?;
    if status != 200 {
        return Err(format!("settings read failed: HTTP {status} {body}"));
    }
    let settings: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
    let source = settings["sources"]
        .as_array()
        .and_then(|sources| sources.iter().find(|source| source["id"] == "github"))
        .ok_or("the plugin exposes no GitHub source")?;
    let revision = source["revision"].as_i64().unwrap_or(0);
    let mut value = source["value"].clone();
    value["enabled"] = json!(true);
    value["config"]["repository"] = json!(repo);
    if !cli.is_empty() {
        value["config"]["cli"] = json!(cli);
    }

    progress("binding the repository (setup check)");
    let payload = json!({ "value": value, "revision": revision }).to_string();
    let (status, body) = api_call(port, "PUT", "settings/source:github", Some(&payload))?;
    if status == 200 {
        return Ok(());
    }
    // 400 carries the setup detail: unreadable repository, or the agent account cannot push.
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|parsed| parsed["setup"]["detail"].as_str().map(str::to_string))
        .unwrap_or(body);
    Err(format!("binding rejected (HTTP {status}): {detail}"))
}

/// Save the two account settings of a running instance: who may ask, and which CLI acts as the agent.
/// The CLI change goes through the plugin's setup check, which may restart it.
fn save_accounts(port: u16, allowed: &[String], cli: &str) -> Result<(), String> {
    let (status, body) = api_call(port, "GET", "settings", None)?;
    if status != 200 {
        return Err(format!("settings read failed: HTTP {status} {body}"));
    }
    let settings: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;

    let block = &settings["blocks"]["policy"];
    let revision = block["revision"].as_i64().unwrap_or(0);
    let mut value = block["value"].clone();
    if !value.is_object() {
        value = json!({ "rules": [], "sources": {} });
    }
    let list: Vec<Value> = allowed
        .iter()
        .map(|login| login.trim())
        .filter(|login| !login.is_empty())
        .map(|login| json!({ "id": login }))
        .collect();
    value["sources"]["github"]["allowed"] = json!(list);
    let payload = json!({ "value": value, "revision": revision }).to_string();
    let (status, body) = api_call(port, "PUT", "settings/policy", Some(&payload))?;
    if status != 200 {
        return Err(format!("policy rejected (HTTP {status}): {body}"));
    }

    let source = settings["sources"]
        .as_array()
        .and_then(|sources| sources.iter().find(|source| source["id"] == "github"))
        .ok_or("the plugin exposes no GitHub source")?;
    let current = source["value"]["config"]["cli"].as_str().unwrap_or_default();
    if !cli.is_empty() && cli != current {
        let revision = source["revision"].as_i64().unwrap_or(0);
        let mut value = source["value"].clone();
        value["config"]["cli"] = json!(cli);
        let payload = json!({ "value": value, "revision": revision }).to_string();
        let (status, body) = api_call(port, "PUT", "settings/source:github", Some(&payload))?;
        if status != 200 {
            return Err(format!("agent cli rejected (HTTP {status}): {body}"));
        }
    }
    Ok(())
}

/// A checkout per ticket. The template instance's `prepare` command points at one repo's local clone, so it
/// cannot be copied; this generic form clones on first use and fetches afterwards. It runs inside the worker
/// environment, where git authenticates as the agent account.
const PREPARE: &str = concat!(
    "if [ -d \"$DSH_SUPERAGENT_DIR/.git\" ]; then ",
    "git -C \"$DSH_SUPERAGENT_DIR\" fetch --no-tags origin; ",
    "else git clone --no-tags \"https://github.com/$DSH_SUPERAGENT_GITHUB_REPOSITORY.git\" \"$DSH_SUPERAGENT_DIR\"; fi"
);

/// Give the new instance a working checkout. Without this the agent gets a ticket and an empty directory.
fn bind_executor(port: u16) -> Result<(), String> {
    let (status, body) = api_call(port, "GET", "settings", None)?;
    if status != 200 {
        return Err(format!("settings read failed: HTTP {status} {body}"));
    }
    let settings: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
    let block = &settings["blocks"]["executor"];
    let revision = block["revision"].as_i64().unwrap_or(0);
    let mut value = block["value"].clone();
    if !value.is_object() {
        return Err("the executor block has no value".into());
    }
    value["prepare"] = json!(PREPARE);
    let payload = json!({ "value": value, "revision": revision }).to_string();
    let (status, body) = api_call(port, "PUT", "settings/executor", Some(&payload))?;
    if status == 200 {
        Ok(())
    } else {
        Err(format!("executor rejected (HTTP {status}): {body}"))
    }
}

/// Policy decides who may ask the agent for work. It is independent of repository access, so it is
/// written before the repository is bound: a failed binding still leaves a usable policy behind.
fn bind_policy(port: u16, login: &str) -> Result<(), String> {
    let (status, body) = api_call(port, "GET", "settings", None)?;
    if status != 200 {
        return Err(format!("settings read failed: HTTP {status} {body}"));
    }
    let settings: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
    let block = &settings["blocks"]["policy"];
    let revision = block["revision"].as_i64().unwrap_or(0);
    let mut value = block["value"].clone();
    if !value.is_object() {
        value = json!({ "rules": [], "sources": {} });
    }
    value["sources"]["github"]["allowed"] = json!([{ "id": login }]);
    let payload = json!({ "value": value, "revision": revision }).to_string();
    let (status, body) = api_call(port, "PUT", "settings/policy", Some(&payload))?;
    if status == 200 {
        Ok(())
    } else {
        Err(format!("policy rejected (HTTP {status}): {body}"))
    }
}

/// The agent CLI as a runnable path: a bare name is looked up in the PATH a profile would see.
fn cli_program(cli: &str) -> PathBuf {
    if cli.contains('/') {
        return PathBuf::from(cli);
    }
    for dir in child_path().split(':') {
        let candidate = Path::new(dir).join(cli);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(cli)
}

/// Run the agent CLI the way the plugin's setup check will, and read the account it reports.
///
/// `Ok(None)` means no CLI was configured and the plugin falls back to its default. A failure here would
/// otherwise surface much later, as a setup rejection after a profile had already been created, so it is
/// checked before anything is written.
fn preflight_cli(cli: &str) -> Result<Option<String>, String> {
    if cli.is_empty() {
        return Ok(None);
    }
    let output = Command::new(cli_program(cli))
        .args(["api", "user", "--jq", ".login"])
        .env("PATH", child_path())
        .output()
        .map_err(|error| format!("cannot run the agent cli {cli}: {error}"))?;
    if !output.status.success() {
        // The wrapper reports its own failure on stderr (`exec: gh: not found`), which is the useful part.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        let detail = if detail.is_empty() {
            match output.status.code() {
                Some(code) => format!("it exited with status {code}"),
                None => "it was stopped by a signal".into(),
            }
        } else {
            detail
        };
        return Err(format!(
            "the agent cli {cli} cannot read its GitHub account: {detail}"
        ));
    }
    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!login.is_empty()).then_some(login))
}

/// A profile left behind by an earlier failed add: it carries this repository's name but nothing is bound.
fn unconfigured_profile(repo: &str) -> Option<Instance> {
    let slug = slug_for(repo);
    discover()
        .into_iter()
        .find(|instance| instance.profile == slug && instance.repo == "not configured")
}

/// The whole "add a repository" job, run off the UI thread.
/// `Result` carries the text shown when it finishes.
///
/// A profile left by an earlier failed add is reused instead of refused: it has nothing bound, and making the
/// operator delete it first would be busywork. A failure after *this* job created the profile removes it again:
/// an "add" is all or nothing, and a half-configured profile would otherwise sit there blocking a retry.
fn add_repository(
    template: String,
    repo: String,
    cli: String,
    human: String,
    tx: Sender<Msg>,
) -> Result<String, String> {
    let progress = |step: &str| {
        let _ = tx.send(Msg::Step(step.to_string()));
    };

    progress("checking the agent cli");
    let agent = preflight_cli(&cli)?;

    let (profile, port, created) = match unconfigured_profile(&repo) {
        Some(instance) => {
            progress(&format!("reusing the unconfigured profile {}", instance.profile));
            (instance.profile, instance.port, false)
        }
        None => {
            progress("creating a profile");
            let (profile, port) = create_profile(&template, &repo, &progress)?;
            (profile, port, true)
        }
    };

    let job = (|| -> Result<(), String> {
        if !is_listening(port) {
            progress("starting it");
            start(&profile)?;
        }
        if !wait_ready(port, &profile, Duration::from_secs(60)) {
            return Err(format!(
                "{profile} did not become ready on {port}; see {}",
                log_for(&profile).display()
            ));
        }

        progress(&format!("allowing {human} to ask for work"));
        bind_policy(port, &human)?;

        progress("binding the repository");
        bind_repository(port, &repo, &cli, &progress)?;

        progress("configuring the per-ticket checkout");
        bind_executor(port)?;

        // The plugin restarts after a successful save; wait for the port to answer again before opening it.
        progress("waiting for the restart");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if is_listening(port) {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    })();

    if let Err(error) = job {
        if created {
            progress("removing the half-made profile");
            return Err(match remove_instance(&profile, true, &progress) {
                Ok(_) => format!("{error} — {profile} was removed again"),
                Err(rollback) => format!("{error} — {profile} could not be removed: {rollback}"),
            });
        }
        return Err(format!("{error} — {profile} kept; fix the problem and retry"));
    }

    open_panel(&profile, port);
    let agent = agent.unwrap_or_else(|| "the configured account".into());
    Ok(format!(
        "{repo} added as {profile} on :{port} — agent {agent}, requester {human}"
    ))
}

struct App {
    instances: Vec<Instance>,
    pending: Option<(String, u16, Instant)>,
    message: Option<(String, Instant)>,
    last_scan: Instant,
    new_repo: String,
    new_cli: String,
    new_human: String,
    job: Option<Receiver<Msg>>,
    job_status: String,
    editing: Option<String>,
    edit_allowed: String,
    edit_cli: String,
    deleting: Option<Deletion>,
}

impl App {
    fn new() -> Self {
        let instances = discover();
        let cli = instances
            .iter()
            .map(|instance| instance.cli.clone())
            .find(|cli| !cli.is_empty())
            .unwrap_or_default();
        Self {
            instances,
            pending: None,
            message: None,
            last_scan: Instant::now(),
            new_repo: String::new(),
            new_cli: cli,
            new_human: "Me106y".into(),
            job: None,
            job_status: String::new(),
            editing: None,
            edit_allowed: String::new(),
            edit_cli: String::new(),
            deleting: None,
        }
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.message = Some((text.into(), Instant::now()));
    }

    /// Take job messages without holding a borrow while mutating the app.
    fn poll_job(&mut self) {
        let Some(receiver) = self.job.take() else {
            return;
        };
        let mut finished = None;
        while let Ok(message) = receiver.try_recv() {
            match message {
                Msg::Step(step) => self.job_status = step,
                Msg::Done(result) => {
                    finished = Some(result);
                    break;
                }
            }
        }
        match finished {
            Some(result) => {
                self.job_status.clear();
                self.instances = discover();
                self.last_scan = Instant::now();
                // A deleted profile may still be the one being edited or waited on; drop that state.
                if let Some(profile) = &self.editing {
                    if !self.instances.iter().any(|instance| &instance.profile == profile) {
                        self.editing = None;
                    }
                }
                if let Some((profile, _, _)) = &self.pending {
                    if !self.instances.iter().any(|instance| &instance.profile == profile) {
                        self.pending = None;
                    }
                }
                match result {
                    Ok(text) | Err(text) => self.notify(text),
                }
            }
            None => self.job = Some(receiver),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_job();

        // Cheap liveness refresh every second, full re-discovery less often.
        if self.last_scan.elapsed() > Duration::from_secs(5) {
            self.instances = discover();
            self.last_scan = Instant::now();
        } else {
            for instance in &mut self.instances {
                instance.running = is_listening(instance.port);
            }
        }

        if let Some((profile, port, at)) = self.pending.clone() {
            if is_listening(port) && last_token(&profile).is_some() {
                open_panel(&profile, port);
                self.notify(format!("{profile} is up on {port}; opening the panel"));
                self.pending = None;
            } else if at.elapsed() > Duration::from_secs(45) {
                self.notify(format!("{profile} did not open port {port} in 45s; see the log"));
                self.pending = None;
            }
        }

        ui.ctx().request_repaint_after(Duration::from_millis(500));

        let mut action: Option<(&'static str, String, u16)> = None;
        let mut add_clicked = false;
        let mut configure: Option<(String, String, String)> = None;
        let mut save_edit: Option<String> = None;
        let mut cancel_edit = false;
        let mut request_delete: Option<String> = None;
        let mut confirm_delete: Option<String> = None;
        let mut cancel_delete = false;
        let busy = self.job.is_some();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading("DSH Superagent");
            ui.label(
                egui::RichText::new("Profiles that mount dsh-superagent. Nothing starts at login.")
                    .weak()
                    .small(),
            );
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new(&self.job_status).small().weak());
                });
            }
            ui.add_space(6.0);

            if self.instances.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 120, 60),
                    format!(
                        "No profile mounts dsh-superagent under {}",
                        profiles_dir().display()
                    ),
                );
            }

            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    for instance in self.instances.clone() {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&instance.profile);
                                if instance.running {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(40, 170, 110),
                                        "running",
                                    );
                                } else {
                                    ui.colored_label(egui::Color32::GRAY, "stopped");
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.monospace(format!(":{}", instance.port));
                                    },
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("repo").weak().small());
                                ui.monospace(&instance.repo);
                            });
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("state").weak().small());
                                ui.label(
                                    egui::RichText::new(instance.state_dir.display().to_string())
                                        .small()
                                        .weak(),
                                );
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(instance.running, egui::Button::new("Open panel"))
                                    .clicked()
                                {
                                    action = Some(("open", instance.profile.clone(), instance.port));
                                }
                                if ui
                                    .add_enabled(!instance.running, egui::Button::new("Start"))
                                    .clicked()
                                {
                                    action = Some(("start", instance.profile.clone(), instance.port));
                                }
                                if ui
                                    .add_enabled(instance.running, egui::Button::new("Stop"))
                                    .clicked()
                                {
                                    action = Some(("stop", instance.profile.clone(), instance.port));
                                }
                            });
                            let confirming = self
                                .deleting
                                .as_ref()
                                .is_some_and(|deletion| deletion.profile == instance.profile);
                            if confirming {
                                ui.label(
                                    egui::RichText::new(format!("Delete {}?", instance.profile))
                                        .strong()
                                        .color(DANGER),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Removes {} and its log. Not reversible.",
                                        profiles_dir().join(&instance.profile).display()
                                    ))
                                    .weak()
                                    .small(),
                                );
                                if instance.running {
                                    ui.label(
                                        egui::RichText::new(
                                            "It is running, so it is stopped first (SIGTERM).",
                                        )
                                        .weak()
                                        .small(),
                                    );
                                }
                                if let Some(deletion) = self.deleting.as_mut() {
                                    ui.checkbox(
                                        &mut deletion.purge_state,
                                        "also delete the state directory",
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} — tickets and per-ticket checkouts",
                                        instance.state_dir.display()
                                    ))
                                    .weak()
                                    .small(),
                                );
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(!busy, egui::Button::new("Cancel"))
                                        .clicked()
                                    {
                                        cancel_delete = true;
                                    }
                                    if ui
                                        .add_enabled(
                                            !busy,
                                            egui::Button::new(
                                                egui::RichText::new("Delete profile")
                                                    .color(DANGER),
                                            ),
                                        )
                                        .clicked()
                                    {
                                        confirm_delete = Some(instance.profile.clone());
                                    }
                                });
                            } else {
                                ui.horizontal(|ui| {
                                    if instance.running {
                                        if ui.button("Configure").clicked() {
                                            configure = Some((
                                                instance.profile.clone(),
                                                instance.allowed.join(", "),
                                                instance.cli.clone(),
                                            ));
                                        }
                                    } else {
                                        ui.add_enabled(false, egui::Button::new("Configure"))
                                            .on_hover_text("start it to edit");
                                    }
                                    if ui
                                        .add_enabled(
                                            !busy,
                                            egui::Button::new(
                                                egui::RichText::new("Delete…").color(DANGER),
                                            ),
                                        )
                                        .on_hover_text("remove this profile")
                                        .clicked()
                                    {
                                        request_delete = Some(instance.profile.clone());
                                    }
                                });
                            }
                            if !confirming
                                && self.editing.as_deref() == Some(instance.profile.as_str())
                            {
                                ui.horizontal(|ui| {
                                    ui.label("allow");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.edit_allowed)
                                            .hint_text("logins that may ask for work, comma separated")
                                            .desired_width(300.0),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    ui.label("cli");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.edit_cli)
                                            .hint_text("gh wrapper signed in as the agent")
                                            .desired_width(300.0),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    if ui.button("Save accounts").clicked() {
                                        save_edit = Some(instance.profile.clone());
                                    }
                                    if ui.button("Cancel").clicked() {
                                        cancel_edit = true;
                                    }
                                });
                            }
                        });
                        ui.add_space(4.0);
                    }
                });

            ui.separator();
            ui.label(egui::RichText::new("Add a repository").strong());
            ui.label(
                egui::RichText::new(
                    "Creates a new profile on a free port, starts it, then binds the repository through the plugin's own setup check.",
                )
                .weak()
                .small(),
            );
            ui.horizontal(|ui| {
                ui.label("owner/name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_repo)
                        .hint_text("acme/widgets")
                        .desired_width(240.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("allow login");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_human)
                        .hint_text("the account allowed to ask for work")
                        .desired_width(240.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("gh cli");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_cli)
                        .hint_text("path to the agent's gh wrapper")
                        .desired_width(320.0),
                );
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("Add and start"))
                    .clicked()
                {
                    add_clicked = true;
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Refresh").clicked() {
                    self.instances = discover();
                    self.last_scan = Instant::now();
                }
                if let Some((text, at)) = &self.message {
                    if at.elapsed() < Duration::from_secs(10) {
                        ui.label(egui::RichText::new(text).small());
                    }
                }
            });
        });

        if add_clicked {
            let repo = self.new_repo.trim().to_string();
            let cli = self.new_cli.trim().to_string();
            let human = self.new_human.trim().to_string();
            let template = self.instances.first().map(|instance| instance.profile.clone());
            match template {
                None => self.notify("add needs one existing profile to copy from"),
                Some(_template) if !valid_repo(&repo) => {
                    self.notify("repository must look like owner/name")
                }
                Some(_template) if self.instances.iter().any(|i| i.repo.eq_ignore_ascii_case(&repo)) => {
                    self.notify(format!("{repo} is already configured"))
                }
                Some(template) => {
                    let (sender, receiver) = mpsc::channel();
                    self.job = Some(receiver);
                    self.job_status = "starting".into();
                    let tx = sender.clone();
                    thread::spawn(move || {
                        let result = add_repository(template, repo, cli, human, tx.clone());
                        let _ = tx.send(Msg::Done(result));
                    });
                }
            }
        }

        if let Some(profile) = request_delete {
            self.deleting = Some(Deletion {
                profile,
                purge_state: false,
            });
        }
        if cancel_delete {
            self.deleting = None;
        }
        if let Some(confirmed) = confirm_delete {
            if self
                .deleting
                .as_ref()
                .is_some_and(|deletion| deletion.profile == confirmed)
            {
                if let Some(deletion) = self.deleting.take() {
                    let purge = deletion.purge_state;
                    let profile = deletion.profile;
                    let (sender, receiver) = mpsc::channel();
                    self.job = Some(receiver);
                    self.job_status = format!("deleting {profile}");
                    let tx = sender.clone();
                    thread::spawn(move || {
                        let result = remove_instance(&profile, purge, &|step| {
                            let _ = tx.send(Msg::Step(step.to_string()));
                        });
                        let _ = tx.send(Msg::Done(result));
                    });
                }
            }
        }

        if let Some((profile, allowed, cli)) = configure {
            self.editing = Some(profile);
            self.edit_allowed = allowed;
            self.edit_cli = cli;
        }
        if cancel_edit {
            self.editing = None;
        }
        if let Some(profile) = save_edit {
            if let Some(instance) = self.instances.iter().find(|i| i.profile == profile) {
                let port = instance.port;
                let allowed: Vec<String> = self
                    .edit_allowed
                    .split(',')
                    .map(|login| login.trim().to_string())
                    .filter(|login| !login.is_empty())
                    .collect();
                let cli = self.edit_cli.trim().to_string();
                let (sender, receiver) = mpsc::channel();
                self.job = Some(receiver);
                self.job_status = "saving accounts".into();
                self.editing = None;
                let tx = sender.clone();
                thread::spawn(move || {
                    let result = save_accounts(port, &allowed, &cli)
                        .map(|()| format!("accounts saved for {profile}"));
                    let _ = tx.send(Msg::Done(result));
                });
            }
        }

        match action {
            Some(("open", profile, port)) => open_panel(&profile, port),
            Some(("start", profile, port)) => match start(&profile) {
                Ok(()) => {
                    self.notify(format!("starting {profile}…"));
                    self.pending = Some((profile, port, Instant::now()));
                }
                Err(error) => self.notify(error),
            },
            Some(("stop", profile, port)) => match stop(&profile, port) {
                Ok(()) => self.notify(format!("stopping {profile} (SIGTERM)")),
                Err(error) => self.notify(error),
            },
            _ => {}
        }
    }
}

fn main() -> eframe::Result<()> {
    // `--list` is the headless view of exactly what the window shows, used by smoke tests.
    if std::env::args().any(|argument| argument == "--list") {
        for instance in discover() {
            println!(
                "{}\t:{}\t{}\t{}\t{}\tallow={}",
                instance.profile,
                instance.port,
                if instance.running { "running" } else { "stopped" },
                instance.repo,
                instance.cli,
                if instance.allowed.is_empty() { "-".to_string() } else { instance.allowed.join(",") }
            );
        }
        return Ok(());
    }

    // `--accounts <port> <logins> <cli>` saves the two account settings headlessly.
    if let Some(index) = std::env::args().position(|argument| argument == "--accounts") {
        let mut rest = std::env::args().skip(index + 1);
        let port = rest.next().and_then(|value| value.parse::<u16>().ok()).unwrap_or(0);
        let allowed: Vec<String> = rest
            .next()
            .unwrap_or_default()
            .split(',')
            .map(|login| login.trim().to_string())
            .filter(|login| !login.is_empty())
            .collect();
        let cli = rest.next().unwrap_or_default();
        match save_accounts(port, &allowed, &cli) {
            Ok(()) => println!("accounts saved on :{port}"),
            Err(text) => {
                eprintln!("{text}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `--remove <profile> [--purge]` deletes one profile without the window, for smoke tests.
    if let Some(index) = std::env::args().position(|argument| argument == "--remove") {
        let profile = std::env::args().nth(index + 1).unwrap_or_default();
        let purge = std::env::args().any(|argument| argument == "--purge");
        match remove_instance(&profile, purge, &|_| {}) {
            Ok(text) => println!("{text}"),
            Err(text) => {
                eprintln!("{text}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `--add owner/name [--allow login] [--cli path]` runs the same job without the window, for smoke tests.
    if let Some(index) = std::env::args().position(|argument| argument == "--add") {
        let repo = std::env::args().nth(index + 1).unwrap_or_default();
        let instances = discover();
        let template = instances
            .first()
            .map(|instance| instance.profile.clone())
            .unwrap_or_default();
        let cli = std::env::args()
            .position(|argument| argument == "--cli")
            .and_then(|index| std::env::args().nth(index + 1))
            .or_else(|| instances.first().map(|instance| instance.cli.clone()))
            .unwrap_or_default();
        let human = std::env::args()
            .position(|argument| argument == "--allow")
            .and_then(|index| std::env::args().nth(index + 1))
            .unwrap_or_else(|| "Me106y".to_string());
        let (tx, _rx) = mpsc::channel();
        match add_repository(template, repo, cli, human, tx) {
            Ok(text) => println!("{text}"),
            Err(text) => {
                eprintln!("{text}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 680.0])
            .with_min_inner_size([560.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DSH Superagent",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
