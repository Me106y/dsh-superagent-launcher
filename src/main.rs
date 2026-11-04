//! DSH Superagent launcher.
//!
//! Finds every DSH profile that mounts `dsh-superagent`, shows the repository it is bound to and the port it
//! serves, and lets you start / open / stop one of them, or add a new repository on a fresh profile and port.
//! No auto-start: nothing here runs at login.
//!
//! Facts that drive the design:
//!
//! * The port lives in `<profile>/cordis.patch.yml` under the `webserver` row.
//! * The repository does **not** live in YAML. It is panel state, stored in `<stateDir>/orchestrator.sqlite`
//!   under the newest `settings-version:N` key. Reading that table is what lets a stopped instance still show
//!   its repository, so this binary links SQLite instead of shelling out to `/usr/bin/sqlite3`, which several
//!   Linux distributions do not ship. A read-only connection cannot be used: the plugin keeps the database in
//!   WAL mode, and a read-only connection has no `-shm` to attach to once the process is gone. A normal
//!   connection with a busy timeout reads safely.
//! * Binding a repository is a plugin operation, not a file edit: `PUT /orchestrator/api/settings/source:github`
//!   runs the plugin's own setup check (repository readable, agent account can push) and restarts it. Adding a
//!   repository any other way would skip that check.
//!
//! UI text is English on purpose: egui ships no CJK font, and bundling a system font is not something this
//! tool should do (redistribution). Labels stay short and technical.

use eframe::egui;
use i18n::{LangKind, fill};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

mod fonts;
mod i18n;
mod platform;
mod settings;
mod theme;

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

fn log_for(profile: &str) -> PathBuf {
    platform::logs_dir().join(format!("dsh-superagent-{profile}.log"))
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
            current = Some(
                rest.trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            );
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
            current = rest
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string();
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
    for line in platform::kv_values(&db) {
        let Ok(parsed) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if parsed["block"] != "source:github" {
            continue;
        }
        let config = &parsed["settings"]["sources"]["github"]["config"];
        let repo = config["repository"]
            .as_str()
            .unwrap_or_default()
            .to_string();
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
    for line in platform::kv_values(&db) {
        let Ok(parsed) = serde_json::from_str::<Value>(&line) else {
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
        let (repo, cli) = settings_from_db(&state_dir)
            .unwrap_or_else(|| ("not configured".into(), String::new()));
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
    let out =
        File::create(&log).map_err(|error| format!("cannot write {}: {error}", log.display()))?;
    let err = out.try_clone().map_err(|error| error.to_string())?;
    let entry = platform::dsh_entry().ok_or_else(|| {
        "cannot find the DeepSeek Harness install (set DSH_INSTALL_DIR to its runtime directory)".to_string()
    })?;
    Command::new(platform::node_bin())
        .arg(entry)
        .arg("--profile")
        .arg(profile)
        .arg("--no-open")
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
        platform::logs_dir().join("dsh-superagent.log"),
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
    platform::open_url(&url);
}

/// SIGTERM, never SIGKILL: the plugin releases its state-directory lock on a clean exit.
///
/// The port is only a hint. Before signalling, the process command line must actually name this profile,
/// so a port reused by something else is never killed by mistake.
fn stop(port: u16, profile: &str) {
    let Ok(output) = Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output()
    else {
        return;
    };
    let marker = format!("--profile {profile}");
    for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        let Ok(command) = Command::new("ps")
            .args(["-p", pid, "-o", "command="])
            .output()
        else {
            continue;
        };
        let command = String::from_utf8_lossy(&command.stdout);
        if !command.contains(&marker) {
            continue;
        }
        let _ = Command::new("kill").args(["-TERM", pid]).status();
    }
}

/// Call the plugin's management API. The header is the same-origin marker the plugin requires.
fn api_call(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let url = format!("http://127.0.0.1:{port}/orchestrator/api/{path}");
    let mut command = Command::new("curl");
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
    let (rest, status) = text
        .rsplit_once("__STATUS__")
        .ok_or("unexpected curl output")?;
    let code = status
        .trim()
        .parse::<u16>()
        .map_err(|_| "unexpected status".to_string())?;
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
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
}

/// The gh wrapper signed in as the agent account.
///
/// Deployment decides this once: `DSH_SUPERAGENT_AGENT_CLI`, the CLI an existing profile already uses, the
/// wrapper the setup guide installs, and finally any other `gh-*` wrapper in `~/.local/bin`.
fn detect_agent_cli(instances: &[Instance]) -> String {
    if let Some(value) = std::env::var_os("DSH_SUPERAGENT_AGENT_CLI") {
        let value = value.to_string_lossy().into_owned();
        if !value.is_empty() {
            return value;
        }
    }
    if let Some(cli) = instances
        .iter()
        .map(|instance| instance.cli.as_str())
        .find(|cli| !cli.is_empty())
    {
        return cli.to_string();
    }
    let bin = platform::home().join(".local/bin");
    let wrapper = bin.join("gh-agent");
    if wrapper.is_file() {
        return wrapper.display().to_string();
    }
    let mut wrappers: Vec<PathBuf> = fs::read_dir(&bin)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("gh-"))
                })
                .filter(|path| path.is_file())
                .collect()
        })
        .unwrap_or_default();
    wrappers.sort();
    wrappers
        .into_iter()
        .next()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

/// Split the "who may ask" field: commas, spaces or newlines, duplicates dropped.
fn parse_logins(text: &str) -> Vec<String> {
    let mut logins: Vec<String> = Vec::new();
    for part in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let login = part.trim();
        if login.is_empty() || logins.iter().any(|seen| seen.eq_ignore_ascii_case(login)) {
            continue;
        }
        logins.push(login.to_string());
    }
    logins
}

/// GitHub logins are alphanumeric with single hyphens inside, at most 39 characters.
fn valid_login(login: &str) -> bool {
    let length = login.chars().count();
    (1..=39).contains(&length)
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
}

fn slug_for(repo: &str) -> String {
    let tail: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
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
fn create_profile(
    template: &str,
    repo: &str,
    progress: &dyn Fn(&str),
) -> Result<(String, u16), String> {
    let name = slug_for(repo);
    let source = profiles_dir().join(template);
    let target = profiles_dir().join(&name);
    if !source.is_dir() {
        return Err(format!("template profile {template} does not exist"));
    }
    if target.exists() {
        return Err(format!("profile {name} already exists"));
    }

    progress("allocating a free port");
    let port = next_free_port(&discover());

    progress("copying the profile");
    let copied = Command::new("cp")
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
    let mut manifest_json: Value =
        serde_json::from_str(&manifest).map_err(|error| error.to_string())?;
    manifest_json["name"] = json!(format!("dsh-profile-{name}"));
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest_json).unwrap_or(manifest),
    )
    .map_err(|error| error.to_string())?;

    let patch_path = target.join("cordis.patch.yml");
    let patch = fs::read_to_string(&patch_path).map_err(|error| error.to_string())?;
    let state_dir = dsh_home().join("state").join(&name);
    let patch = set_block_scalar(&patch, "webserver", "port", &port.to_string())
        .ok_or("the template has no webserver port row")?;
    let patch = set_block_scalar(
        &patch,
        "dsh-superagent",
        "stateDir",
        &state_dir.display().to_string(),
    )
    .ok_or("the template has no dsh-superagent stateDir row")?;
    fs::write(&patch_path, patch).map_err(|error| error.to_string())?;
    fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;

    Ok((name, port))
}

/// Bind the repository through the plugin's own settings API, which runs the setup check and restarts it.
fn bind_repository(
    port: u16,
    repo: &str,
    cli: &str,
    progress: &dyn Fn(&str),
) -> Result<(), String> {
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

/// Save who may ask on a running instance.
fn save_accounts(port: u16, allowed: &[String]) -> Result<(), String> {
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
fn bind_policy(port: u16, allowed: &[String]) -> Result<(), String> {
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
    let list: Vec<Value> = allowed.iter().map(|login| json!({ "id": login })).collect();
    value["sources"]["github"]["allowed"] = json!(list);
    let payload = json!({ "value": value, "revision": revision }).to_string();
    let (status, body) = api_call(port, "PUT", "settings/policy", Some(&payload))?;
    if status == 200 {
        Ok(())
    } else {
        Err(format!("policy rejected (HTTP {status}): {body}"))
    }
}

/// Ask the agent CLI which GitHub account it is signed in as, purely for the confirmation message.
fn agent_login(cli: &str) -> Option<String> {
    if cli.is_empty() {
        return None;
    }
    let output = Command::new(cli)
        .args(["api", "user", "--jq", ".login"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!login.is_empty()).then_some(login)
}

/// The whole "add a repository" job, run off the UI thread.
/// `Result` carries the text shown when it finishes.
fn add_repository(
    template: String,
    repo: String,
    cli: String,
    who: Vec<String>,
    lang: LangKind,
    tx: Sender<Msg>,
) -> Result<String, String> {
    let t = i18n::texts(lang);
    if cli.is_empty() {
        return Err(t.no_agent_cli.to_string());
    }
    let human = who.join(", ");
    let progress = |step: &str| {
        let _ = tx.send(Msg::Step(step.to_string()));
    };
    progress(t.step_create);
    let (profile, port) = create_profile(&template, &repo, &progress)?;

    progress(t.step_start);
    start(&profile)?;
    if !wait_ready(port, &profile, Duration::from_secs(60)) {
        return Err(fill(
            t.not_ready,
            &[
                ("profile", &profile),
                ("port", &port.to_string()),
                ("log", &log_for(&profile).display().to_string()),
            ],
        ));
    }

    progress(&fill(t.step_allow, &[("who", &human)]));
    bind_policy(port, &who)?;

    progress(t.step_bind);
    bind_repository(port, &repo, &cli, &progress)?;

    progress(t.step_checkout);
    bind_executor(port)?;

    // The plugin restarts after a successful save; wait for the port to answer again before opening it.
    progress(t.step_restart);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if is_listening(port) {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    open_panel(&profile, port);
    let agent = agent_login(&cli).unwrap_or_else(|| cli.clone());
    Ok(fill(
        t.added,
        &[
            ("repo", &repo),
            ("profile", &profile),
            ("port", &port.to_string()),
            ("agent", &agent),
            ("human", &human),
        ],
    ))
}

/// How this launch differs from the saved preferences, used by `--screenshot` and by hand.
#[derive(Clone, Default)]
struct Launch {
    screenshot: Option<PathBuf>,
    theme: Option<theme::ThemeChoice>,
    lang: Option<i18n::Lang>,
}

struct App {
    screenshot: Option<PathBuf>,
    frames: u32,
    instances: Vec<Instance>,
    pending: Option<(String, u16, Instant)>,
    message: Option<(String, Instant)>,
    last_scan: Instant,
    new_repo: String,
    new_who: String,
    job: Option<Receiver<Msg>>,
    job_status: String,
    editing: Option<String>,
    edit_allowed: String,
    theme: theme::ThemeChoice,
    lang: i18n::Lang,
    cjk: bool,
    cjk_checked: bool,
    dark: bool,
    applied: Option<theme::ThemeChoice>,
    agent_cli: String,
}

impl App {
    fn new(launch: Launch) -> Self {
        let instances = discover();
        let mut saved = settings::load();
        if let Some(theme) = launch.theme {
            saved.theme = theme;
        }
        if let Some(lang) = launch.lang {
            saved.lang = lang;
        }
        let who = instances
            .iter()
            .find(|instance| !instance.allowed.is_empty())
            .map(|instance| instance.allowed.join(", "))
            .unwrap_or_default();
        let agent_cli = detect_agent_cli(&instances);
        Self {
            screenshot: launch.screenshot,
            frames: 0,
            instances,
            pending: None,
            message: None,
            last_scan: Instant::now(),
            new_repo: String::new(),
            new_who: who,
            job: None,
            job_status: String::new(),
            editing: None,
            edit_allowed: String::new(),
            theme: saved.theme,
            lang: saved.lang,
            cjk: fonts::available(),
            cjk_checked: false,
            dark: true,
            applied: None,
            agent_cli,
        }
    }

    /// The language the labels are drawn in. `System` follows the locale; without a CJK font, English.
    fn language(&self) -> LangKind {
        let kind = self.lang.resolve();
        if kind == LangKind::Zh && !self.cjk {
            LangKind::En
        } else {
            kind
        }
    }

    fn save_preferences(&self) {
        settings::save(settings::Settings {
            theme: self.theme,
            lang: self.lang,
        });
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
                match result {
                    Ok(text) => self.notify(text),
                    Err(text) => self.notify(text),
                }
            }
            None => self.job = Some(receiver),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // `Auto` follows the desktop scheme; reapplying only on change keeps egui's animations intact.
        let ctx = ui.ctx().clone();
        if self.applied != Some(self.theme) {
            theme::apply(&ctx, self.theme);
            self.applied = Some(self.theme);
        }
        let dark = ctx.theme() == egui::Theme::Dark;
        self.dark = dark;
        let palette = theme::palette(dark);
        // A font file that exists may still lack the glyphs; check once, on the first frame.
        if !self.cjk_checked {
            self.cjk = self.cjk
                && ctx.fonts_mut(|fonts| {
                    fonts.has_glyphs(&egui::FontId::proportional(14.0), "中文仓库")
                });
            self.cjk_checked = true;
        }
        let lang = self.language();
        let t = i18n::texts(lang);

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
                self.notify(fill(
                    t.opened,
                    &[("profile", &profile), ("port", &port.to_string())],
                ));
                self.pending = None;
            } else if at.elapsed() > Duration::from_secs(45) {
                self.notify(fill(
                    t.open_timeout,
                    &[("profile", &profile), ("port", &port.to_string())],
                ));
                self.pending = None;
            }
        }

        if let Some(path) = self.screenshot.clone() {
            self.frames += 1;
            if self.frames == 12 {
                let context = ctx.clone();
                ctx.request_screenshot(move |image| {
                    if let Err(error) = save_png(&path, &image) {
                        eprintln!("screenshot failed: {error}");
                    }
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                });
            }
            if self.frames > 120 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        ui.ctx().request_repaint_after(Duration::from_millis(500));

        let mut action: Option<(&'static str, String, u16)> = None;
        let mut add_clicked = false;
        let mut configure: Option<(String, String)> = None;
        let mut save_edit: Option<String> = None;
        let mut cancel_edit = false;

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("DSH Superagent");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Right-to-left: the first widget added ends up rightmost.
                    if self.cjk {
                        let (next, label) = match lang {
                            LangKind::Zh => (i18n::Lang::En, "EN"),
                            LangKind::En => (i18n::Lang::Zh, "中文"),
                        };
                        if ui
                            .button(egui::RichText::new(label).small())
                            .on_hover_text("切换界面语言 / Switch language")
                            .clicked()
                        {
                            self.lang = next;
                            self.save_preferences();
                        }
                        ui.add_space(8.0);
                    }
                    for (choice, index) in [
                        (theme::ThemeChoice::Dark, 2),
                        (theme::ThemeChoice::Light, 1),
                        (theme::ThemeChoice::System, 0),
                    ] {
                        if ui
                            .selectable_label(
                                self.theme == choice,
                                egui::RichText::new(t.theme_labels[index]).small(),
                            )
                            .clicked()
                        {
                            self.theme = choice;
                            self.save_preferences();
                        }
                    }
                });
            });
            ui.label(egui::RichText::new(t.subtitle).weak().small());
            ui.add_space(6.0);

            if self.instances.is_empty() {
                ui.colored_label(
                    palette.warn,
                    fill(
                        t.no_profiles,
                        &[("profiles", &profiles_dir().display().to_string())],
                    ),
                );
            }

            egui::ScrollArea::vertical()
                .max_height(250.0)
                .show(ui, |ui| {
                    for instance in self.instances.clone() {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&instance.profile);
                                if instance.running {
                                    ui.colored_label(palette.accent, t.running);
                                } else {
                                    ui.colored_label(palette.weak, t.stopped);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.monospace(format!(":{}", instance.port));
                                    },
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(t.repository).weak().small());
                                let repository = if instance.repo == "not configured" {
                                    t.not_configured
                                } else {
                                    &instance.repo
                                };
                                ui.monospace(repository);
                            });
                            if !instance.cli.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(t.agent_cli).weak().small());
                                    ui.monospace(&instance.cli);
                                });
                            }
                            if !instance.allowed.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(t.who_may_ask).weak().small());
                                    ui.label(
                                        egui::RichText::new(instance.allowed.join(", ")).small(),
                                    );
                                });
                            }
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(t.state_dir).weak().small());
                                ui.label(
                                    egui::RichText::new(instance.state_dir.display().to_string())
                                        .small()
                                        .weak(),
                                );
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(instance.running, egui::Button::new(t.open_panel))
                                    .clicked()
                                {
                                    action =
                                        Some(("open", instance.profile.clone(), instance.port));
                                }
                                if ui
                                    .add_enabled(!instance.running, egui::Button::new(t.start))
                                    .clicked()
                                {
                                    action =
                                        Some(("start", instance.profile.clone(), instance.port));
                                }
                                if ui
                                    .add_enabled(instance.running, egui::Button::new(t.stop))
                                    .clicked()
                                {
                                    action =
                                        Some(("stop", instance.profile.clone(), instance.port));
                                }
                                if instance.running {
                                    if ui.button(t.configure).clicked() {
                                        configure = Some((
                                            instance.profile.clone(),
                                            instance.allowed.join(", "),
                                        ));
                                    }
                                } else {
                                    ui.add_enabled(false, egui::Button::new(t.configure))
                                        .on_hover_text(t.configure_hint);
                                }
                            });
                            if self.editing.as_deref() == Some(instance.profile.as_str()) {
                                ui.horizontal(|ui| {
                                    ui.label(t.who_label).on_hover_text(t.who_tooltip);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.edit_allowed)
                                            .hint_text(t.who_hint)
                                            .desired_width(280.0),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    if ui.button(t.save).clicked() {
                                        save_edit = Some(instance.profile.clone());
                                    }
                                    if ui.button(t.cancel).clicked() {
                                        cancel_edit = true;
                                    }
                                });
                            }
                        });
                        ui.add_space(4.0);
                    }
                });

            ui.separator();
            ui.label(egui::RichText::new(t.add_title).strong());
            ui.label(egui::RichText::new(t.add_description).weak().small());
            ui.add_space(2.0);
            egui::Grid::new("add-repository")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label(t.repo_label).on_hover_text(t.repo_tooltip);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_repo)
                            .hint_text(t.repo_hint)
                            .desired_width(300.0),
                    );
                    ui.end_row();

                    ui.label(t.who_label).on_hover_text(t.who_tooltip);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_who)
                            .hint_text(t.who_hint)
                            .desired_width(300.0),
                    );
                    ui.end_row();
                });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(t.agent_cli).weak().small());
                if self.agent_cli.is_empty() {
                    ui.label(
                        egui::RichText::new(t.no_agent_cli)
                            .small()
                            .color(palette.warn),
                    );
                } else {
                    ui.monospace(&self.agent_cli)
                        .on_hover_text("Everything the agent writes appears under this account.");
                }
            });
            ui.horizontal(|ui| {
                let busy = self.job.is_some();
                if ui
                    .add_enabled(
                        !busy && !self.agent_cli.is_empty(),
                        egui::Button::new(
                            egui::RichText::new(t.add_and_start)
                                .color(palette.on_accent)
                                .strong(),
                        )
                        .fill(palette.accent),
                    )
                    .clicked()
                {
                    add_clicked = true;
                }
                if busy {
                    ui.spinner();
                    ui.label(egui::RichText::new(&self.job_status).small());
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(t.refresh).clicked() {
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
            let who = parse_logins(&self.new_who);
            let template = self
                .instances
                .first()
                .map(|instance| instance.profile.clone());
            let invalid = who.iter().find(|login| !valid_login(login)).cloned();
            match template {
                None => self.notify(t.need_template),
                Some(_) if !valid_repo(&repo) => self.notify(t.invalid_repo),
                Some(_) if who.is_empty() => self.notify(fill(t.invalid_login, &[("login", "")])),
                Some(_) if invalid.is_some() => self.notify(fill(
                    t.invalid_login,
                    &[("login", invalid.as_deref().unwrap_or_default())],
                )),
                Some(_)
                    if self
                        .instances
                        .iter()
                        .any(|instance| instance.repo.eq_ignore_ascii_case(&repo)) =>
                {
                    self.notify(fill(t.already_configured, &[("repo", &repo)]))
                }
                Some(_) if self.agent_cli.is_empty() => self.notify(t.no_agent_cli),
                Some(template) => {
                    let (sender, receiver) = mpsc::channel();
                    self.job = Some(receiver);
                    self.job_status = t.working.into();
                    let tx = sender.clone();
                    let cli = self.agent_cli.clone();
                    thread::spawn(move || {
                        let result = add_repository(template, repo, cli, who, lang, tx.clone());
                        let _ = tx.send(Msg::Done(result));
                    });
                }
            }
        }

        if let Some((profile, allowed)) = configure {
            self.editing = Some(profile);
            self.edit_allowed = allowed;
        }
        if cancel_edit {
            self.editing = None;
        }
        if let Some(profile) = save_edit {
            if let Some(instance) = self.instances.iter().find(|i| i.profile == profile) {
                let port = instance.port;
                let allowed = parse_logins(&self.edit_allowed);
                let invalid = allowed.iter().find(|login| !valid_login(login)).cloned();
                if allowed.is_empty() || invalid.is_some() {
                    self.notify(fill(
                        t.invalid_login,
                        &[("login", invalid.as_deref().unwrap_or_default())],
                    ));
                } else {
                    let (sender, receiver) = mpsc::channel();
                    self.job = Some(receiver);
                    self.job_status = t.working.into();
                    self.editing = None;
                    let tx = sender.clone();
                    let profile = profile.clone();
                    thread::spawn(move || {
                        let result = save_accounts(port, &allowed)
                            .map(|()| fill(t.accounts_saved, &[("profile", &profile)]));
                        let _ = tx.send(Msg::Done(result));
                    });
                }
            }
        }

        match action {
            Some(("open", profile, port)) => open_panel(&profile, port),
            Some(("start", profile, port)) => match start(&profile) {
                Ok(()) => {
                    self.notify(fill(t.starting, &[("profile", &profile)]));
                    self.pending = Some((profile, port, Instant::now()));
                }
                Err(error) => self.notify(error),
            },
            Some(("stop", profile, port)) => {
                stop(port, &profile);
                self.notify(fill(t.stopping, &[("profile", &profile)]));
            }
            _ => {}
        }
    }
}

/// Write a screenshot as an 8-bit RGBA PNG, the format the README images use.
fn save_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        image.size[0] as u32,
        image.size[1] as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        bytes.extend_from_slice(&pixel.to_array());
    }
    writer
        .write_image_data(&bytes)
        .map_err(|error| error.to_string())
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn main() -> eframe::Result<()> {
    // `--list` is the headless view of exactly what the window shows, used by smoke tests.
    if std::env::args().any(|argument| argument == "--list") {
        for instance in discover() {
            println!(
                "{}\t:{}\t{}\t{}\t{}\tallow={}",
                instance.profile,
                instance.port,
                if instance.running {
                    "running"
                } else {
                    "stopped"
                },
                instance.repo,
                instance.cli,
                if instance.allowed.is_empty() {
                    "-".to_string()
                } else {
                    instance.allowed.join(",")
                }
            );
        }
        return Ok(());
    }

    // `--diagnose` prints the resolved preferences and the detected agent CLI, for bug reports.
    if std::env::args().any(|argument| argument == "--diagnose") {
        let saved = settings::load();
        println!(
            "theme={} lang={:?} cjk={} agent_cli={}",
            saved.theme.stored(),
            saved.lang.resolve(),
            fonts::available(),
            detect_agent_cli(&discover())
        );
        return Ok(());
    }

    // `--accounts <port> <logins>` saves who may ask, headlessly.
    if let Some(index) = std::env::args().position(|argument| argument == "--accounts") {
        let mut rest = std::env::args().skip(index + 1);
        let port = rest
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        let allowed = parse_logins(&rest.next().unwrap_or_default());
        match save_accounts(port, &allowed) {
            Ok(()) => println!("accounts saved on :{port}"),
            Err(text) => {
                eprintln!("{text}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `--add owner/name [--allow a,b]` runs the same job without the window, for smoke tests.
    if let Some(index) = std::env::args().position(|argument| argument == "--add") {
        let repo = std::env::args().nth(index + 1).unwrap_or_default();
        let instances = discover();
        let template = instances
            .first()
            .map(|instance| instance.profile.clone())
            .unwrap_or_default();
        let cli = detect_agent_cli(&instances);
        let who = std::env::args()
            .position(|argument| argument == "--allow")
            .and_then(|index| std::env::args().nth(index + 1))
            .map(|value| parse_logins(&value))
            .unwrap_or_default();
        let (tx, _rx) = mpsc::channel();
        match add_repository(template, repo, cli, who, LangKind::En, tx) {
            Ok(text) => println!("{text}"),
            Err(text) => {
                eprintln!("{text}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let args: Vec<String> = std::env::args().collect();
    let launch = Launch {
        screenshot: arg_value(&args, "--screenshot").map(PathBuf::from),
        theme: arg_value(&args, "--theme").and_then(|value| theme::ThemeChoice::parse(&value)),
        lang: arg_value(&args, "--lang").and_then(|value| i18n::Lang::parse(&value)),
    };
    let capturing = launch.screenshot.is_some();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // The capture runs at zoom 2, so the window must be twice the logical size.
            .with_inner_size(if capturing {
                [1320.0, 1010.0]
            } else {
                [680.0, 720.0]
            })
            .with_min_inner_size([560.0, 480.0])
            .with_app_id("dsh-superagent-launcher"),
        ..Default::default()
    };
    eframe::run_native(
        "DSH Superagent",
        options,
        Box::new(move |cc| {
            if capturing {
                // 2x for a README image that stays sharp on a high-density display.
                cc.egui_ctx.set_zoom_factor(2.0);
            }
            fonts::install(&cc.egui_ctx);
            Ok(Box::new(App::new(launch)))
        }),
    )
}
