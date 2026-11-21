//! Slash commands (same names as Hermes where they overlap).

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    New,
    Retry,
    Undo,
    Compress,
    Usage,
    Help,
    Stop,
    Yolo(Option<bool>),
    /// `/model` lists; `/model name` or `/model provider:name` switches.
    Model(Option<String>),
    Provider(Option<String>),
    Personality(Option<String>),
    Title(String),
    Skills,
    /// `/<skill-name> [instructions]`
    Skill {
        name: String,
        rest: String,
    },
    Sessions,
    Search(String),
    Memory,
    Tools,
    Cwd(Option<String>),
    /// `/rollback` lists file changes; `/rollback N` undoes the last N.
    Rollback(Option<usize>),
    /// `/insights [days]`
    Insights(Option<i64>),
    /// `/mcp` shows MCP servers and their tools.
    Mcp,
    Unknown(String),
}

pub const HELP: &[(&str, &str)] = &[
    ("/new, /reset", "start a fresh conversation"),
    ("/model [provider:]name", "show or switch the model"),
    ("/provider [id]", "show or switch the provider"),
    ("/retry", "drop the last answer and ask again"),
    ("/undo", "remove the last exchange"),
    ("/compress", "summarise older turns to free context"),
    ("/usage", "token usage for this session"),
    ("/personality [name]", "show or set the personality"),
    ("/title text", "rename this session"),
    ("/skills", "list skills; /<skill-name> runs one"),
    ("/sessions", "recent sessions"),
    ("/search text", "search past sessions"),
    ("/memory", "show what's remembered"),
    ("/tools", "show enabled tools"),
    ("/cwd [path]", "show or change the working directory"),
    ("/rollback [n]", "list file edits, or undo the last n"),
    ("/insights [days]", "usage over recent days"),
    ("/mcp", "MCP servers and their tools"),
    ("/yolo [on|off]", "toggle approval for dangerous commands"),
    ("/stop", "interrupt the current turn"),
    ("/help", "this list"),
];

pub fn help_text() -> String {
    let w = HELP.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    HELP.iter().map(|(c, d)| format!("{c:<w$}  {d}")).collect::<Vec<_>>().join("\n")
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Parse input. Returns `None` for ordinary messages (including `//escaped`).
pub fn parse(input: &str, skill_names: &[String]) -> Option<Command> {
    let t = input.trim_start();
    if !t.starts_with('/') || t.starts_with("//") {
        return None;
    }
    let body = &t[1..];
    let (cmd, rest) = match body.find(char::is_whitespace) {
        Some(i) => (&body[..i], body[i..].trim()),
        None => (body, ""),
    };
    // A path like `/etc/hosts is broken` is a message, not a command.
    if cmd.is_empty() || cmd.contains('/') {
        return None;
    }
    let lc = cmd.to_ascii_lowercase();
    Some(match lc.as_str() {
        "new" | "reset" | "clear" => Command::New,
        "retry" => Command::Retry,
        "undo" => Command::Undo,
        "compress" => Command::Compress,
        "usage" | "cost" => Command::Usage,
        "help" | "?" => Command::Help,
        "stop" => Command::Stop,
        "yolo" => Command::Yolo(match rest.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => Some(true),
            "off" | "false" | "0" => Some(false),
            _ => None,
        }),
        "model" | "models" => Command::Model(opt(rest)),
        "provider" | "providers" => Command::Provider(opt(rest)),
        "personality" => Command::Personality(opt(rest)),
        "title" | "rename" => Command::Title(rest.to_string()),
        "skills" => Command::Skills,
        "sessions" | "history" => Command::Sessions,
        "search" => Command::Search(rest.to_string()),
        "memory" => Command::Memory,
        "tools" => Command::Tools,
        "cwd" | "cd" => Command::Cwd(opt(rest)),
        "rollback" | "checkpoints" => Command::Rollback(rest.trim().parse().ok()),
        "insights" | "stats" => Command::Insights(rest.trim().trim_start_matches("--days").trim().parse().ok()),
        "mcp" => Command::Mcp,
        _ => match skill_names.iter().find(|s| s.eq_ignore_ascii_case(cmd)) {
            Some(name) => Command::Skill { name: name.clone(), rest: rest.to_string() },
            None => Command::Unknown(cmd.to_string()),
        },
    })
}

/// `provider:model` → (Some(provider), model); `model` → (None, model). Model ids may contain
/// colons themselves (`qwen3:8b`), so the prefix only counts when it names a known provider.
pub fn split_model_spec(spec: &str, provider_ids: &[String]) -> (Option<String>, String) {
    if let Some((p, m)) = spec.split_once(':')
        && provider_ids.iter().any(|id| id == p)
    {
        return (Some(p.to_string()), m.to_string());
    }
    (None, spec.to_string())
}

/// Text sent to the model for `/<skill> rest`.
pub fn skill_invocation(name: &str, body: &str, rest: &str) -> String {
    let mut s = format!("Use the \"{name}\" skill for this. Its instructions:\n\n<skill name=\"{name}\">\n{}\n</skill>\n", body.trim());
    if !rest.trim().is_empty() {
        s.push_str(&format!("\nTask: {}", rest.trim()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let skills = vec!["deploy-app".to_string()];
        assert_eq!(parse("hello", &skills), None);
        assert_eq!(parse("//not a command", &skills), None);
        assert_eq!(parse("/etc/hosts is broken", &skills), None);
        assert_eq!(parse("/reset", &skills), Some(Command::New));
        assert_eq!(parse("  /model  ollama:qwen3:8b ", &skills), Some(Command::Model(Some("ollama:qwen3:8b".into()))));
        assert_eq!(parse("/model", &skills), Some(Command::Model(None)));
        assert_eq!(parse("/yolo off", &skills), Some(Command::Yolo(Some(false))));
        assert_eq!(parse("/yolo", &skills), Some(Command::Yolo(None)));
        assert_eq!(parse("/Deploy-App to staging", &skills), Some(Command::Skill { name: "deploy-app".into(), rest: "to staging".into() }));
        assert_eq!(parse("/frobnicate", &skills), Some(Command::Unknown("frobnicate".into())));
        assert_eq!(parse("/search gitea proxy", &skills), Some(Command::Search("gitea proxy".into())));
        assert_eq!(parse("/rollback 2", &skills), Some(Command::Rollback(Some(2))));
        assert_eq!(parse("/rollback", &skills), Some(Command::Rollback(None)));
        assert_eq!(parse("/insights --days 7", &skills), Some(Command::Insights(Some(7))));
        assert_eq!(parse("/mcp", &skills), Some(Command::Mcp));
    }

    #[test]
    fn model_specs() {
        let ids = vec!["ollama".to_string(), "nous".to_string()];
        assert_eq!(split_model_spec("ollama:qwen3:8b", &ids), (Some("ollama".into()), "qwen3:8b".into()));
        assert_eq!(split_model_spec("qwen3:8b", &ids), (None, "qwen3:8b".into()));
        assert_eq!(split_model_spec("Hermes-4-70B", &ids), (None, "Hermes-4-70B".into()));
    }

    #[test]
    fn help_lists_everything() {
        let h = help_text();
        for (c, _) in HELP {
            assert!(h.contains(c));
        }
    }
}
