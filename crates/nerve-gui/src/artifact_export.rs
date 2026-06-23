//! Copyable artifacts shared by the command palette and review/context surfaces.

use crate::{
    app::{Chat, Role},
    clipboard::copy_text_with_note,
    context_manifest::{handoff_text, manifest_text},
    data::{fetch_context, fetch_diff, fetch_file_tree, save_host_text_file, selection_summary},
    diff_review::diff_review_packet,
    trace_format::tool_trace,
};
use leptos::prelude::*;

pub(crate) fn copy_thread_transcript(
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    note: RwSignal<String>,
) {
    let transcript =
        chats.with_untracked(|items| thread_transcript(items.get(active.get_untracked())));
    copy_text_with_note(transcript, note, "Copied active thread transcript.");
}

pub(crate) fn copy_tool_activity(
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    note: RwSignal<String>,
) {
    let packet =
        chats.with_untracked(|items| tool_activity_packet(items.get(active.get_untracked())));
    copy_text_with_note(packet, note, "Copied tool activity.");
}

pub(crate) fn save_tool_activity(
    token: StoredValue<Option<String>>,
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save tool activity.".into());
        return;
    };
    let (packet, title) = chats.with_untracked(|items| {
        let chat = items.get(active.get_untracked());
        (
            tool_activity_packet(chat),
            chat.map_or_else(|| "thread".to_string(), |item| item.title.clone()),
        )
    });
    let file_name = artifact_file_name("nerve-thread", &title, "tool-activity.md");
    save_text_artifact(
        tok,
        "Save tool activity",
        file_name,
        packet,
        "Saved tool activity",
        note,
    );
}

pub(crate) fn save_thread_transcript(
    token: StoredValue<Option<String>>,
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save thread transcript.".into());
        return;
    };
    let (transcript, title) = chats.with_untracked(|items| {
        let chat = items.get(active.get_untracked());
        (
            thread_transcript(chat),
            chat.map_or_else(|| "thread".to_string(), |item| item.title.clone()),
        )
    });
    let file_name = artifact_file_name("nerve-thread", &title, "transcript.md");
    save_text_artifact(
        tok,
        "Save thread transcript",
        file_name,
        transcript,
        "Saved thread transcript",
        note,
    );
}

pub(crate) fn copy_selection_manifest(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot read selection.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Copying selection manifest…".into());
    leptos::task::spawn_local(async move {
        let summary = selection_summary(&tok, &ws).await;
        let file_count = summary.files.len();
        copy_text_with_note(
            manifest_text(&summary),
            note,
            format!("Copied selection manifest for {file_count} files."),
        );
    });
}

pub(crate) fn save_selection_manifest(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save selection manifest.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Building selection manifest…".into());
    leptos::task::spawn_local(async move {
        let summary = selection_summary(&tok, &ws).await;
        let file_count = summary.files.len();
        let file_name = artifact_file_name("nerve", &ws, "selection-manifest.md");
        let success = format!("Saved selection manifest for {file_count} files");
        let message = save_text_artifact_message(
            &tok,
            "Save selection manifest",
            &file_name,
            manifest_text(&summary),
            &success,
        )
        .await;
        note.set(message);
    });
}

pub(crate) fn copy_context_handoff(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot build context handoff.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Building context handoff…".into());
    leptos::task::spawn_local(async move {
        let summary = selection_summary(&tok, &ws).await;
        let context = fetch_context(&tok, "standard", None, &ws)
            .await
            .map(|(text, _)| text)
            .unwrap_or_default();
        let file_count = summary.files.len();
        copy_text_with_note(
            handoff_text(&summary, &ws, "standard", &context),
            note,
            format!("Copied context handoff for {file_count} files."),
        );
    });
}

pub(crate) fn save_context_handoff(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save context handoff.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Building context handoff…".into());
    leptos::task::spawn_local(async move {
        let summary = selection_summary(&tok, &ws).await;
        let context = fetch_context(&tok, "standard", None, &ws)
            .await
            .map(|(text, _)| text)
            .unwrap_or_default();
        let file_count = summary.files.len();
        let file_name = artifact_file_name("nerve", &ws, "context-handoff.md");
        let success = format!("Saved context handoff for {file_count} files");
        let message = save_text_artifact_message(
            &tok,
            "Save context handoff",
            &file_name,
            handoff_text(&summary, &ws, "standard", &context),
            &success,
        )
        .await;
        note.set(message);
    });
}

pub(crate) fn copy_review_packet(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot build review packet.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Building review packet…".into());
    leptos::task::spawn_local(async move {
        let diff = fetch_diff(&tok, &ws)
            .await
            .unwrap_or_else(|| "No diff available.".into());
        copy_text_with_note(diff_review_packet(&diff), note, "Copied review packet.");
    });
}

pub(crate) fn save_review_packet(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save review packet.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Building review packet…".into());
    leptos::task::spawn_local(async move {
        let diff = fetch_diff(&tok, &ws)
            .await
            .unwrap_or_else(|| "No diff available.".into());
        let file_name = artifact_file_name("nerve", &ws, "review-packet.md");
        let message = save_text_artifact_message(
            &tok,
            "Save review packet",
            &file_name,
            diff_review_packet(&diff),
            "Saved review packet",
        )
        .await;
        note.set(message);
    });
}

pub(crate) fn copy_file_tree(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot read file tree.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Copying file tree…".into());
    leptos::task::spawn_local(async move {
        let tree = fetch_file_tree(&tok, &ws)
            .await
            .unwrap_or_else(|| "No file tree available.".into());
        copy_text_with_note(file_tree_packet(&ws, &tree), note, "Copied file tree.");
    });
}

pub(crate) fn save_file_tree(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save file tree.".into());
        return;
    };
    let ws = workspace.get_untracked();
    note.set("Reading file tree…".into());
    leptos::task::spawn_local(async move {
        let tree = fetch_file_tree(&tok, &ws)
            .await
            .unwrap_or_else(|| "No file tree available.".into());
        let file_name = artifact_file_name("nerve", &ws, "file-tree.md");
        let message = save_text_artifact_message(
            &tok,
            "Save file tree",
            &file_name,
            file_tree_packet(&ws, &tree),
            "Saved file tree",
        )
        .await;
        note.set(message);
    });
}

pub(crate) fn copy_full_handoff_bundle(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot build handoff bundle.".into());
        return;
    };
    let ws = workspace.get_untracked();
    let transcript =
        chats.with_untracked(|items| thread_transcript(items.get(active.get_untracked())));
    let activity =
        chats.with_untracked(|items| tool_activity_packet(items.get(active.get_untracked())));
    note.set("Building full handoff bundle…".into());
    leptos::task::spawn_local(async move {
        let bundle = full_handoff_bundle(&tok, &ws, transcript, activity).await;
        copy_text_with_note(bundle, note, "Copied full handoff bundle.");
    });
}

pub(crate) fn save_full_handoff_bundle(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    note: RwSignal<String>,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot save handoff bundle.".into());
        return;
    };
    let ws = workspace.get_untracked();
    let transcript =
        chats.with_untracked(|items| thread_transcript(items.get(active.get_untracked())));
    let activity =
        chats.with_untracked(|items| tool_activity_packet(items.get(active.get_untracked())));
    note.set("Building full handoff bundle…".into());
    leptos::task::spawn_local(async move {
        let bundle = full_handoff_bundle(&tok, &ws, transcript, activity).await;
        let file_name = artifact_file_name("nerve", &ws, "full-handoff-bundle.md");
        let message = save_text_artifact_message(
            &tok,
            "Save full handoff bundle",
            &file_name,
            bundle,
            "Saved full handoff bundle",
        )
        .await;
        note.set(message);
    });
}

async fn full_handoff_bundle(
    token: &str,
    workspace: &str,
    transcript: String,
    activity: String,
) -> String {
    let summary = selection_summary(token, workspace).await;
    let context = fetch_context(token, "standard", None, workspace)
        .await
        .map(|(text, _)| text)
        .unwrap_or_default();
    let diff = fetch_diff(token, workspace)
        .await
        .unwrap_or_else(|| "No diff available.".into());
    let tree = fetch_file_tree(token, workspace)
        .await
        .unwrap_or_else(|| "No file tree available.".into());
    format!(
        "# Nerve handoff bundle\n\nWorkspace: {workspace}\n\n---\n\n{}\n\n---\n\n{}\n\n---\n\n{}\n\n---\n\n{}\n\n---\n\n# Workspace file tree\n\n```text\n{}\n```",
        transcript,
        activity,
        handoff_text(&summary, workspace, "standard", &context),
        diff_review_packet(&diff),
        tree
    )
}

fn save_text_artifact(
    token: String,
    title: &'static str,
    file_name: String,
    text: String,
    success: &'static str,
    note: RwSignal<String>,
) {
    note.set("Opening save panel…".into());
    leptos::task::spawn_local(async move {
        let message = save_text_artifact_message(&token, title, &file_name, text, success).await;
        note.set(message);
    });
}

async fn save_text_artifact_message(
    token: &str,
    title: &str,
    file_name: &str,
    text: String,
    success: &str,
) -> String {
    match save_host_text_file(token, title, file_name, text).await {
        Ok(path) => format!("{success} to {path}."),
        Err(err) if err.to_ascii_lowercase().contains("cancel") => "Save cancelled.".into(),
        Err(err) => format!("Save failed: {err}"),
    }
}

fn artifact_file_name(prefix: &str, source: &str, suffix: &str) -> String {
    let source = truncate_file_name_segment(&file_name_segment(source, "workspace"), 48);
    format!("{prefix}-{source}-{suffix}")
}

fn file_tree_packet(workspace: &str, tree: &str) -> String {
    format!("# Workspace file tree\n\nWorkspace: {workspace}\n\n```text\n{tree}\n```")
}

fn file_name_segment(value: &str, fallback: &'static str) -> String {
    let segment: String = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect();
    let trimmed = segment.trim_matches('-');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn truncate_file_name_segment(segment: &str, max_chars: usize) -> String {
    segment.chars().take(max_chars).collect()
}

fn thread_transcript(chat: Option<&Chat>) -> String {
    let Some(chat) = chat else {
        return "# Thread transcript\n\nNo active thread.".into();
    };
    let mut out = format!("# Thread transcript\n\nTitle: {}\n", chat.title);
    if let Some(session) = &chat.session {
        out.push_str(&format!("Session: {session}\n"));
    }
    if chat.turns.is_empty() {
        out.push_str("\nNo messages yet.\n");
        return out;
    }
    for (index, turn) in chat.turns.iter().enumerate() {
        out.push_str(&format!("\n## {} {}\n\n", index + 1, role_label(turn.role)));
        if !turn.text.trim().is_empty() {
            out.push_str(turn.text.trim());
            out.push('\n');
        }
        if !turn.reasoning.trim().is_empty() {
            out.push_str("\n### Reasoning\n\n");
            out.push_str(turn.reasoning.trim());
            out.push('\n');
        }
        for tool in &turn.tools {
            let status = match tool.ok {
                None => "running",
                Some(true) => "ok",
                Some(false) => "error",
            };
            out.push_str("\n### Tool trace\n\n```text\n");
            out.push_str(&tool_trace(&tool.tool, status, &tool.input, &tool.output));
            out.push_str("\n```\n");
        }
    }
    out
}

fn tool_activity_packet(chat: Option<&Chat>) -> String {
    let Some(chat) = chat else {
        return "# Tool activity\n\nNo active thread.".into();
    };
    let tools = chat.turns.iter().flat_map(|turn| turn.tools.iter());
    let mut total = 0usize;
    let mut ok = 0usize;
    let mut err = 0usize;
    let mut run = 0usize;
    let mut sections = Vec::new();
    for (index, tool) in tools.enumerate() {
        total += 1;
        let status = match tool.ok {
            None => {
                run += 1;
                "running"
            }
            Some(true) => {
                ok += 1;
                "ok"
            }
            Some(false) => {
                err += 1;
                "error"
            }
        };
        sections.push(format!(
            "## #{:02} {} — {}\n\n{}",
            index + 1,
            tool.tool,
            status,
            tool_trace(&tool.tool, status, &tool.input, &tool.output)
        ));
    }
    let mut out = format!(
        "# Tool activity\n\n{} tool calls · {} ok · {} errors · {} running",
        total, ok, err, run
    );
    if sections.is_empty() {
        out.push_str("\n\nNo tool activity in this thread yet.");
    } else {
        out.push_str("\n\n");
        out.push_str(&sections.join("\n\n"));
    }
    out
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    }
}
