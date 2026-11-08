//! Recognising the coding agents and bots credited on a commit, and drawing
//! their marks.

use gpui_kit::assets::IconName;
use gpui_kit::*;

use super::labels;
use crate::theme::color;

/// Share of an avatar's diameter that a mark fills.
const MARK_SCALE: f32 = 0.62;

/// How to recognise an agent or bot, and its mark.
struct Account {
    icon: &'static str,
    background: u32,
    /// Whole addresses, in lower case.
    emails: &'static [&'static str],
    /// GitHub logins, in lower case and without `[bot]`.
    logins: &'static [&'static str],
    /// Whole names, in lower case and without `[bot]`. Only names that are
    /// not also people's: "Claude" and "Cursor" alone are left to the email.
    names: &'static [&'static str],
    /// Beginnings of names, in lower case.
    name_prefixes: &'static [&'static str],
}

const NOTHING: Account = Account {
    icon: "",
    background: color::AVATAR,
    emails: &[],
    logins: &[],
    names: &[],
    name_prefixes: &[],
};

/// Addresses of agents with no freely licensed mark, drawn as a robot.
const UNMARKED_AGENTS: [&str; 2] = ["aider@aider.chat", "amp@ampcode.com"];

/// Every account with a mark. `crate::assets` embeds the icons.
const ACCOUNTS: [Account; 14] = [
    Account {
        icon: "agents/claude.svg",
        background: color::AGENT_CLAUDE,
        emails: &["noreply@anthropic.com", "claude@anthropic.com"],
        logins: &["claude"],
        names: &["claude code"],
        name_prefixes: &[
            "claude opus",
            "claude sonnet",
            "claude haiku",
            "claude fable",
        ],
    },
    Account {
        icon: "agents/codex.svg",
        emails: &["codex@openai.com", "noreply@openai.com"],
        logins: &["codex", "chatgpt-codex-connector"],
        names: &["codex", "openai codex", "codex cli"],
        ..NOTHING
    },
    Account {
        icon: "agents/copilot.svg",
        emails: &["copilot@github.com"],
        logins: &["copilot", "copilot-swe-agent", "github-copilot"],
        names: &["copilot", "github copilot"],
        ..NOTHING
    },
    Account {
        icon: "agents/cursor.svg",
        emails: &["cursoragent@cursor.com", "agent@cursor.com"],
        logins: &["cursor", "cursoragent"],
        names: &["cursor agent", "cursoragent"],
        ..NOTHING
    },
    Account {
        icon: "agents/gemini.svg",
        background: color::AGENT_GEMINI,
        emails: &["gemini-cli@google.com"],
        logins: &["gemini-cli", "gemini-code-assist"],
        names: &["gemini cli", "gemini code assist", "gemini-code-assist"],
        ..NOTHING
    },
    Account {
        icon: "agents/windsurf.svg",
        emails: &[
            "windsurf@codeium.com",
            "noreply@windsurf.com",
            "cascade@windsurf.com",
        ],
        logins: &["windsurf"],
        names: &["windsurf cascade"],
        ..NOTHING
    },
    Account {
        icon: "agents/devin.svg",
        emails: &["devin@cognition.ai"],
        logins: &["devin-ai-integration"],
        names: &["devin ai", "devin-ai-integration"],
        ..NOTHING
    },
    Account {
        icon: "agents/opencode.svg",
        emails: &["noreply@opencode.ai", "opencode@sst.dev"],
        logins: &["opencode-agent"],
        names: &["opencode"],
        ..NOTHING
    },
    Account {
        icon: "agents/openhands.svg",
        emails: &["openhands@all-hands.dev"],
        logins: &["openhands-agent", "openhands-ai"],
        names: &["openhands", "openhands-agent"],
        ..NOTHING
    },
    Account {
        icon: "agents/github-actions.svg",
        background: color::AGENT_GITHUB_ACTIONS,
        emails: &["action@github.com", "actions@github.com"],
        logins: &["github-actions"],
        names: &["github-actions", "github actions"],
        ..NOTHING
    },
    Account {
        icon: "agents/dependabot.svg",
        background: color::AGENT_DEPENDABOT,
        emails: &["support@dependabot.com"],
        logins: &["dependabot", "dependabot-preview"],
        names: &["dependabot", "dependabot-preview"],
        ..NOTHING
    },
    Account {
        icon: "agents/renovate.svg",
        background: color::AGENT_RENOVATE,
        emails: &["bot@renovateapp.com"],
        logins: &["renovate", "renovate-bot"],
        names: &["renovate", "renovate-bot"],
        ..NOTHING
    },
    Account {
        icon: "agents/coderabbit.svg",
        background: color::AGENT_CODERABBIT,
        logins: &["coderabbitai"],
        names: &["coderabbitai", "coderabbit"],
        ..NOTHING
    },
    Account {
        icon: "agents/github.svg",
        emails: &["noreply@github.com"],
        names: &["github"],
        ..NOTHING
    },
];

/// The round avatar of the person or agent called `name` at `email`: the
/// mark of a known agent or bot, a robot for any other automated account,
/// and the initials of anyone else.
pub fn avatar(name: &str, email: &str, diameter: Pixels) -> AnyElement {
    match mark(name, email) {
        Some(mark) => div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(diameter)
            .rounded_full()
            .bg(rgb(mark.background))
            .child(
                svg()
                    .path(mark.path)
                    .size(diameter * MARK_SCALE)
                    .text_color(rgb(mark.foreground)),
            )
            .into_any_element(),
        None => labels::avatar(name, diameter).into_any_element(),
    }
}

/// What to draw for an account that is not a person.
#[derive(Debug, PartialEq, Eq)]
struct Mark {
    path: SharedString,
    background: u32,
    foreground: u32,
}

/// The mark for `name` at `email`, or `None` for a person.
fn mark(name: &str, email: &str) -> Option<Mark> {
    let email = email.trim().to_lowercase();
    let name = name.trim().to_lowercase();
    let bare_name = name.strip_suffix("[bot]").unwrap_or(&name).trim_end();
    let local = email.split('@').next().unwrap_or_default();
    // `<id>+<login>@users.noreply.github.com`, or the older form without id.
    let login = email
        .strip_suffix("@users.noreply.github.com")
        .and_then(|local| local.rsplit('+').next())
        .map(|login| login.strip_suffix("[bot]").unwrap_or(login));

    let matches = |account: &Account| {
        account.emails.contains(&email.as_str())
            || login.is_some_and(|login| account.logins.contains(&login))
            || account.names.contains(&bare_name)
            || account
                .name_prefixes
                .iter()
                .any(|prefix| bare_name.starts_with(prefix))
    };
    if let Some(account) = ACCOUNTS.iter().find(|account| matches(account)) {
        return Some(Mark {
            path: account.icon.into(),
            background: account.background,
            foreground: color::AGENT_MARK,
        });
    }

    let automated = name.ends_with("[bot]")
        || UNMARKED_AGENTS.contains(&email.as_str())
        || local.ends_with("[bot]")
        || matches!(local, "noreply" | "no-reply");
    automated.then(|| Mark {
        path: IconName::Bot.path(),
        background: color::AVATAR,
        foreground: color::AVATAR_TEXT,
    })
}

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName;

    use super::{ACCOUNTS, mark};

    fn icon(name: &str, email: &str) -> Option<String> {
        mark(name, email).map(|mark| mark.path.to_string())
    }

    fn agent(name: &str) -> Option<String> {
        Some(format!("agents/{name}.svg"))
    }

    #[test]
    fn every_mark_is_embedded() {
        for account in &ACCOUNTS {
            assert!(
                matches!(crate::assets::Assets.load(account.icon), Ok(Some(_))),
                "{} is not embedded",
                account.icon
            );
        }
    }

    #[test]
    fn agents_are_recognised_by_their_address() {
        assert_eq!(
            icon("Claude Opus 5.5", "noreply@anthropic.com"),
            agent("claude")
        );
        assert_eq!(icon("Whatever", "CODEX@openai.com"), agent("codex"));
        assert_eq!(
            icon("Cursor Agent", "cursoragent@cursor.com"),
            agent("cursor")
        );
    }

    #[test]
    fn github_logins_are_recognised_with_or_without_an_id() {
        assert_eq!(
            icon(
                "github-actions[bot]",
                "41898282+github-actions[bot]@users.noreply.github.com"
            ),
            agent("github-actions")
        );
        assert_eq!(
            icon("Copilot", "175728472+Copilot@users.noreply.github.com"),
            agent("copilot")
        );
        assert_eq!(
            icon("x", "dependabot[bot]@users.noreply.github.com"),
            agent("dependabot")
        );
    }

    #[test]
    fn product_names_are_recognised_without_an_address() {
        assert_eq!(icon("Claude Sonnet 4.5", ""), agent("claude"));
        assert_eq!(icon("renovate[bot]", ""), agent("renovate"));
        assert_eq!(icon("Codex", "someone@example.com"), agent("codex"));
    }

    #[test]
    fn people_keep_their_initials() {
        assert_eq!(icon("Ada Lovelace", "ada@example.com"), None);
        assert_eq!(icon("Claude Monet", "claude@example.com"), None);
        assert_eq!(icon("Cursor", "cursor@example.com"), None);
        assert_eq!(icon("Ada", "1234+ada@users.noreply.github.com"), None);
    }

    #[test]
    fn other_automated_accounts_get_the_robot() {
        let robot = Some(IconName::Bot.path().to_string());
        assert_eq!(icon("release-please[bot]", ""), robot);
        assert_eq!(icon("x", "55+stale[bot]@users.noreply.github.com"), robot);
        assert_eq!(icon("aider (gpt-5)", "noreply@aider.chat"), robot);
        assert_eq!(icon("Amp", "amp@ampcode.com"), robot);
    }
}
