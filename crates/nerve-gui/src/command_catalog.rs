//! Static command catalog and search helpers for the command palette.

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandTone {
    Default,
    Primary,
    Danger,
}

pub(crate) struct Command {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) desc: &'static str,
    pub(crate) tags: &'static str,
    pub(crate) key: &'static str,
    pub(crate) tone: CommandTone,
}

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        id: "01",
        title: "Plan next change",
        desc: "Codex-style: inspect, plan, then act",
        tags: "codex inspect plan act",
        key: "↵",
        tone: CommandTone::Primary,
    },
    Command {
        id: "02",
        title: "Build context",
        desc: "RepoPrompt-style file selection, token budget, handoff",
        tags: "repoprompt context selection tokens handoff",
        key: "⌘2",
        tone: CommandTone::Default,
    },
    Command {
        id: "03",
        title: "Open review packet",
        desc: "Diff review cockpit + copyable handoff packet",
        tags: "diff review packet handoff inspector",
        key: "⌘3",
        tone: CommandTone::Default,
    },
    Command {
        id: "04",
        title: "Open tool activity",
        desc: "Inspect and copy the thread's tool-call packet",
        tags: "tools activity trace packet inspector handoff",
        key: "⌘4",
        tone: CommandTone::Default,
    },
    Command {
        id: "25",
        title: "Open agents",
        desc: "Inspect running CLI agents across threads",
        tags: "agents sessions cli claude codex inspector running",
        key: "agents",
        tone: CommandTone::Default,
    },
    Command {
        id: "05",
        title: "Draft review prompt",
        desc: "Seed the composer with a diff review request",
        tags: "diff review prompt risks tests",
        key: "review ↵",
        tone: CommandTone::Default,
    },
    Command {
        id: "06",
        title: "Toggle inspector",
        desc: "Files, changes, and tool activity",
        tags: "inspector files changes tools",
        key: "⌘I",
        tone: CommandTone::Default,
    },
    Command {
        id: "07",
        title: "New thread",
        desc: "Start a clean delegated session",
        tags: "thread session new",
        key: "⌘N",
        tone: CommandTone::Default,
    },
    Command {
        id: "08",
        title: "Clear current thread",
        desc: "Close its delegate session and reset history",
        tags: "clear reset close",
        key: "⌘⌫",
        tone: CommandTone::Danger,
    },
    Command {
        id: "09",
        title: "Find in current view",
        desc: "Focus thread search or the context file filter",
        tags: "find search threads sidebar context files filter",
        key: "⌘F",
        tone: CommandTone::Default,
    },
    Command {
        id: "10",
        title: "Open settings",
        desc: "Configure provider, model, theme, and autonomy",
        tags: "settings preferences provider model theme autonomy",
        key: "⌘,",
        tone: CommandTone::Default,
    },
    Command {
        id: "11",
        title: "Copy thread transcript",
        desc: "Markdown transcript with reasoning and tool traces",
        tags: "copy transcript thread trace tools handoff",
        key: "copy thread",
        tone: CommandTone::Default,
    },
    Command {
        id: "12",
        title: "Copy context handoff",
        desc: "Assemble and copy a RepoPrompt-style standard handoff",
        tags: "copy context repoprompt handoff selection artifact",
        key: "⇧⌘C",
        tone: CommandTone::Primary,
    },
    Command {
        id: "13",
        title: "Copy selection manifest",
        desc: "Copy selected files, modes, ranges, and token estimates",
        tags: "copy selection manifest files tokens ranges",
        key: "manifest",
        tone: CommandTone::Default,
    },
    Command {
        id: "14",
        title: "Copy review packet",
        desc: "Fetch the working diff and copy a review-ready packet",
        tags: "copy review packet diff checklist prompt",
        key: "review packet",
        tone: CommandTone::Default,
    },
    Command {
        id: "15",
        title: "Copy tool activity",
        desc: "Copy the active thread's tool-call timeline",
        tags: "copy tools activity trace timeline packet",
        key: "tool activity",
        tone: CommandTone::Default,
    },
    Command {
        id: "16",
        title: "Copy file tree",
        desc: "Fetch and copy the workspace tree for handoff",
        tags: "copy files tree workspace handoff context",
        key: "file tree",
        tone: CommandTone::Default,
    },
    Command {
        id: "17",
        title: "Copy full handoff bundle",
        desc: "Thread, context, review, tools, and file tree in one packet",
        tags: "copy full handoff bundle repoprompt codex context review tools files",
        key: "⇧⌘B",
        tone: CommandTone::Primary,
    },
    Command {
        id: "18",
        title: "Save full handoff bundle",
        desc: "Export thread, context, review, tools, and file tree with a native save panel",
        tags: "save export full handoff bundle repoprompt codex context review tools files",
        key: "save bundle",
        tone: CommandTone::Primary,
    },
    Command {
        id: "19",
        title: "Save context handoff",
        desc: "Export a RepoPrompt-style standard handoff with a native save panel",
        tags: "save export context repoprompt handoff selection artifact",
        key: "save context",
        tone: CommandTone::Primary,
    },
    Command {
        id: "20",
        title: "Save review packet",
        desc: "Export the working diff review packet with a native save panel",
        tags: "save export review packet diff checklist prompt",
        key: "save review",
        tone: CommandTone::Default,
    },
    Command {
        id: "21",
        title: "Save thread transcript",
        desc: "Export the active thread transcript with a native save panel",
        tags: "save export transcript thread trace tools handoff",
        key: "save thread",
        tone: CommandTone::Default,
    },
    Command {
        id: "22",
        title: "Save selection manifest",
        desc: "Export selected files, modes, ranges, and token estimates",
        tags: "save export selection manifest files tokens ranges",
        key: "save manifest",
        tone: CommandTone::Default,
    },
    Command {
        id: "23",
        title: "Save tool activity",
        desc: "Export the active thread's tool-call timeline",
        tags: "save export tools activity trace timeline packet",
        key: "save tools",
        tone: CommandTone::Default,
    },
    Command {
        id: "24",
        title: "Save file tree",
        desc: "Export the workspace tree for handoff",
        tags: "save export files tree workspace handoff context",
        key: "save tree",
        tone: CommandTone::Default,
    },
];

pub(crate) fn active_command(
    query: &str,
    active: usize,
    native_file_dialogs: bool,
) -> Option<&'static str> {
    visible_commands(query, native_file_dialogs)
        .get(active)
        .map(|command| command.id)
}

pub(crate) fn active_option_id(query: &str, active: usize, native_file_dialogs: bool) -> String {
    active_command(query, active, native_file_dialogs)
        .map(command_option_id)
        .unwrap_or_default()
}

pub(crate) fn command_option_id(id: &str) -> String {
    format!("cmd-command-{id}")
}

pub(crate) fn visible_commands(query: &str, native_file_dialogs: bool) -> Vec<&'static Command> {
    COMMANDS
        .iter()
        .filter(|command| command_available(command, native_file_dialogs))
        .filter(|command| command_matches(query, command))
        .collect()
}

fn command_available(command: &Command, native_file_dialogs: bool) -> bool {
    native_file_dialogs || !matches!(command.id, "18" | "19" | "20" | "21" | "22" | "23" | "24")
}

fn command_matches(query: &str, command: &Command) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || q.split_whitespace().all(|part| {
            command.title.to_lowercase().contains(part)
                || command.desc.to_lowercase().contains(part)
                || command.tags.to_lowercase().contains(part)
        })
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, visible_commands};

    #[test]
    fn agents_command_is_registered_and_searchable() {
        assert!(COMMANDS.iter().any(|command| command.id == "25"));
        let ids: Vec<_> = visible_commands("agents", true)
            .into_iter()
            .map(|command| command.id)
            .collect();
        assert!(ids.contains(&"25"));
        // The runtime-session commands (26-31) were removed in the cockpit pivot.
        for id in ["26", "27", "28", "29", "30", "31"] {
            assert!(!COMMANDS.iter().any(|command| command.id == id), "{id}");
        }
    }

    #[test]
    fn native_file_dialog_filter_hides_save_commands() {
        let ids: Vec<_> = visible_commands("", false)
            .into_iter()
            .map(|command| command.id)
            .collect();
        for id in ["18", "19", "20", "21", "22", "23", "24"] {
            assert!(!ids.contains(&id), "{id}");
        }
        assert!(ids.contains(&"25"));
    }
}
