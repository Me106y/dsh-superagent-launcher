//! The two interface languages and every string the window shows.
//!
//! A struct of `&'static str` fields instead of a runtime map: the compiler then catches a string that exists in
//! one language and not the other. Messages with values use `{token}` placeholders and [`fill`], so the two
//! languages may order the tokens differently.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Lang {
    #[default]
    System,
    Zh,
    En,
}

/// The resolved language, after `System` is matched against the desktop locale.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LangKind {
    En,
    Zh,
}

impl Lang {
    pub fn resolve(self) -> LangKind {
        match self {
            Lang::Zh => LangKind::Zh,
            Lang::En => LangKind::En,
            Lang::System => {
                if system_locale_is_chinese() {
                    LangKind::Zh
                } else {
                    LangKind::En
                }
            }
        }
    }

    pub fn stored(self) -> &'static str {
        match self {
            Lang::System => "system",
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Lang::System),
            "zh" | "zh-CN" | "zh-Hans" => Some(Lang::Zh),
            "en" => Some(Lang::En),
            _ => None,
        }
    }
}

/// Swap `{token}` placeholders for their values.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in args {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

fn system_locale_is_chinese() -> bool {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        return value.to_ascii_lowercase().starts_with("zh");
    }
    false
}

pub struct Texts {
    pub subtitle: &'static str,
    pub running: &'static str,
    pub stopped: &'static str,
    pub repository: &'static str,
    pub agent_cli: &'static str,
    pub state_dir: &'static str,
    pub who_may_ask: &'static str,
    pub not_configured: &'static str,
    pub open_panel: &'static str,
    pub start: &'static str,
    pub stop: &'static str,
    pub configure: &'static str,
    pub configure_hint: &'static str,
    pub save: &'static str,
    pub cancel: &'static str,
    pub add_title: &'static str,
    pub add_description: &'static str,
    pub repo_label: &'static str,
    pub repo_hint: &'static str,
    pub repo_tooltip: &'static str,
    pub who_label: &'static str,
    pub who_hint: &'static str,
    pub who_tooltip: &'static str,
    pub add_and_start: &'static str,
    pub refresh: &'static str,
    pub theme_labels: [&'static str; 3],
    pub no_profiles: &'static str,
    pub invalid_repo: &'static str,
    pub already_configured: &'static str,
    pub need_template: &'static str,
    pub starting: &'static str,
    pub stopping: &'static str,
    pub opened: &'static str,
    pub open_timeout: &'static str,
    pub accounts_saved: &'static str,
    pub step_create: &'static str,
    pub step_start: &'static str,
    pub step_allow: &'static str,
    pub step_bind: &'static str,
    pub step_checkout: &'static str,
    pub step_restart: &'static str,
    pub added: &'static str,
    pub no_agent_cli: &'static str,
    pub invalid_login: &'static str,
    pub working: &'static str,
    pub not_ready: &'static str,
    pub bind_repo: &'static str,
    pub bind_ok: &'static str,
    pub agent_no_push: &'static str,
    pub repo_unreachable: &'static str,
    pub bind_hint: &'static str,
    pub dismiss: &'static str,
}

pub fn texts(kind: LangKind) -> &'static Texts {
    match kind {
        LangKind::En => &EN,
        LangKind::Zh => &ZH,
    }
}

static EN: Texts = Texts {
    subtitle: "Profiles that mount dsh-superagent. Nothing starts at login.",
    running: "running",
    stopped: "stopped",
    repository: "repository",
    agent_cli: "agent CLI",
    state_dir: "state dir",
    who_may_ask: "who may ask",
    not_configured: "not configured",
    open_panel: "Open panel",
    start: "Start",
    stop: "Stop",
    configure: "Configure",
    configure_hint: "start it to edit",
    save: "Save",
    cancel: "Cancel",
    add_title: "Add a repository",
    add_description: "Creates a new profile on a free port, starts it, then binds the repository through the plugin's own setup check.",
    repo_label: "Repository",
    repo_hint: "owner/name, e.g. Me106y/dsh-job-ledger",
    repo_tooltip: "The GitHub repository the agent works on.",
    who_label: "Who may ask",
    who_hint: "GitHub logins, comma separated",
    who_tooltip: "Only these GitHub accounts can ask the agent for work.",
    add_and_start: "Add and start",
    refresh: "Refresh",
    theme_labels: ["Auto", "Light", "Dark"],
    no_profiles: "No profile mounts dsh-superagent under {profiles}",
    invalid_repo: "repository must look like owner/name, for example Me106y/dsh-job-ledger",
    already_configured: "{repo} is already configured",
    need_template: "add needs one existing profile to copy from",
    starting: "starting {profile}…",
    stopping: "stopping {profile} (SIGTERM)",
    opened: "{profile} is up on {port}; opening the panel",
    open_timeout: "{profile} did not open port {port} in 45s; see the log",
    accounts_saved: "accounts saved for {profile}",
    step_create: "creating a profile",
    step_start: "starting it",
    step_allow: "allowing {who} to ask for work",
    step_bind: "binding the repository (setup check)",
    step_checkout: "configuring the per-ticket checkout",
    step_restart: "waiting for the restart",
    added: "{repo} added as {profile} on :{port} — agent {agent}, requester {human}",
    no_agent_cli: "no agent gh wrapper found; install it at ~/.local/bin/gh-agent or set DSH_SUPERAGENT_AGENT_CLI",
    invalid_login: "{login} is not a GitHub login",
    working: "working…",
    not_ready: "{profile} did not become ready on :{port}; see {log}",
    bind_repo: "Bind repository",
    bind_ok: "{repo} is now bound to {profile}",
    agent_no_push: "{agent} cannot push to {repo}.",
    repo_unreachable: "{repo} does not exist, or {agent} cannot see it.",
    bind_hint: "Invite the agent account as a collaborator on GitHub, accept the invite, then bind again.",
    dismiss: "Dismiss",
};

static ZH: Texts = Texts {
    subtitle: "挂载 dsh-superagent 的 profile。不会随登录自启。",
    running: "运行中",
    stopped: "已停止",
    repository: "仓库",
    agent_cli: "Agent CLI",
    state_dir: "状态目录",
    who_may_ask: "可派活的人",
    not_configured: "未配置",
    open_panel: "打开面板",
    start: "启动",
    stop: "停止",
    configure: "配置",
    configure_hint: "启动后才能编辑",
    save: "保存",
    cancel: "取消",
    add_title: "添加仓库",
    add_description: "在空闲端口新建 profile 并启动，然后通过插件自身的部署检查绑定仓库。",
    repo_label: "仓库",
    repo_hint: "owner/name，例如 Me106y/dsh-job-ledger",
    repo_tooltip: "agent 要工作的 GitHub 仓库。",
    who_label: "谁可以派活",
    who_hint: "GitHub 登录名，逗号分隔",
    who_tooltip: "只有这些 GitHub 账号可以让 agent 干活。",
    add_and_start: "添加并启动",
    refresh: "刷新",
    theme_labels: ["自动", "浅色", "深色"],
    no_profiles: "没有 profile 挂载 dsh-superagent：{profiles}",
    invalid_repo: "仓库要写成 owner/name，例如 Me106y/dsh-job-ledger",
    already_configured: "{repo} 已经配置过了",
    need_template: "添加仓库需要先有一个可复制的 profile",
    starting: "正在启动 {profile}…",
    stopping: "正在停止 {profile}（SIGTERM）",
    opened: "{profile} 已在 :{port} 就绪，正在打开面板",
    open_timeout: "{profile} 45 秒内没有打开端口 {port}，请查看日志",
    accounts_saved: "已保存 {profile} 的配置",
    step_create: "创建 profile",
    step_start: "启动",
    step_allow: "允许 {who} 派活",
    step_bind: "绑定仓库（会跑部署检查）",
    step_checkout: "配置每个工单的 checkout",
    step_restart: "等待重启",
    added: "已把 {repo} 添加为 {profile}，端口 :{port} — agent 账号 {agent}，可派活 {human}",
    no_agent_cli: "没找到 agent 用的 gh 包装脚本：按文档放到 ~/.local/bin/gh-agent，或设置 DSH_SUPERAGENT_AGENT_CLI",
    invalid_login: "{login} 不是合法的 GitHub 登录名",
    working: "处理中…",
    not_ready: "{profile} 没能在 :{port} 就绪，见 {log}",
    bind_repo: "绑定仓库",
    bind_ok: "{repo} 已绑定到 {profile}",
    agent_no_push: "agent 账号 {agent} 对 {repo} 没有 push 权限。",
    repo_unreachable: "仓库 {repo} 不存在，或 agent 账号 {agent} 看不到它。",
    bind_hint: "先在 GitHub 上把 agent 账号加为 collaborator 并接受邀请，然后重新绑定。",
    dismiss: "关闭",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_replaces_every_token() {
        let text = fill("{a} and {b}", &[("a", "1"), ("b", "2")]);
        assert_eq!(text, "1 and 2");
    }

    #[test]
    fn every_language_round_trips_through_the_config_file() {
        for lang in [Lang::System, Lang::Zh, Lang::En] {
            assert_eq!(Lang::parse(lang.stored()), Some(lang));
        }
    }

    #[test]
    fn both_languages_define_the_same_labels() {
        for (en, zh) in [
            (EN.add_title, ZH.add_title),
            (EN.who_label, ZH.who_label),
            (EN.repo_label, ZH.repo_label),
        ] {
            assert!(!en.is_empty() && !zh.is_empty());
            assert_ne!(en, zh);
        }
    }
}
