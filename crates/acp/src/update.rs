//! `session/update` notifications as types: what the agent says, thinks, plans and does.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A file change the agent proposes or made (`oldText` None: a new file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

/// What a tool call carries: text, a diff, or a terminal's id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolContent {
    Text(String),
    Diff(Diff),
    Terminal(String),
}

/// A tool call, or the fields an update changes (None: unchanged).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCall {
    pub id: String,
    pub title: Option<String>,
    /// read, edit, delete, move, search, execute, think, fetch, switch_mode, other.
    pub kind: Option<String>,
    /// pending, in_progress, completed, failed.
    pub status: Option<String>,
    /// Replaces the call's content when set.
    pub content: Option<Vec<ToolContent>>,
    /// Files it touches: (path, 1-based line).
    pub locations: Option<Vec<(String, Option<u64>)>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    /// pending, in_progress, completed.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    UserText(String),
    AgentText(String),
    Thought(String),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCall),
    /// The whole plan (each update replaces it).
    Plan(Vec<PlanEntry>),
    /// Slash commands the agent offers: (name, description).
    Commands(Vec<(String, String)>),
    /// Anything else (mode changes, usage...).
    Other(String),
}

/// The text of a content block (resources and links by their name or URI).
pub fn block_text(block: &Value) -> String {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().unwrap_or("").to_string(),
        Some("resource_link") => format!("[{}]", block["name"].as_str().or(block["uri"].as_str()).unwrap_or("")),
        Some("resource") => format!("[{}]", block["resource"]["uri"].as_str().unwrap_or("")),
        Some("image") => "[image]".into(),
        Some("audio") => "[audio]".into(),
        _ => String::new(),
    }
}

fn tool_call(v: &Value) -> ToolCall {
    let content = v["content"].as_array().map(|items| {
        items
            .iter()
            .filter_map(|c| match c["type"].as_str()? {
                "content" => Some(ToolContent::Text(block_text(&c["content"]))),
                "diff" => Some(ToolContent::Diff(Diff {
                    path: c["path"].as_str()?.to_string(),
                    old_text: c["oldText"].as_str().map(String::from),
                    new_text: c["newText"].as_str()?.to_string(),
                })),
                "terminal" => Some(ToolContent::Terminal(c["terminalId"].as_str()?.to_string())),
                _ => None,
            })
            .collect()
    });
    let locations = v["locations"].as_array().map(|items| items.iter().filter_map(|l| Some((l["path"].as_str()?.to_string(), l["line"].as_u64()))).collect());
    let s = |k: &str| v[k].as_str().map(String::from);
    ToolCall { id: s("toolCallId").unwrap_or_default(), title: s("title"), kind: s("kind"), status: s("status"), content, locations }
}

/// Reads a `session/update`'s `update` object.
pub fn parse(update: &Value) -> Update {
    let kind = update["sessionUpdate"].as_str().unwrap_or("");
    match kind {
        "user_message_chunk" => Update::UserText(block_text(&update["content"])),
        "agent_message_chunk" => Update::AgentText(block_text(&update["content"])),
        "agent_thought_chunk" => Update::Thought(block_text(&update["content"])),
        "tool_call" => Update::ToolCall(tool_call(update)),
        "tool_call_update" => Update::ToolCallUpdate(tool_call(update)),
        "plan" => Update::Plan(
            update["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| Some(PlanEntry { content: e["content"].as_str()?.to_string(), status: e["status"].as_str().unwrap_or("pending").to_string() }))
                .collect(),
        ),
        "available_commands_update" => Update::Commands(
            update["availableCommands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["description"].as_str().unwrap_or("").to_string())))
                .collect(),
        ),
        other => Update::Other(other.to_string()),
    }
}

/// A permission option: (id, name, kind: allow_once, allow_always, reject_once, reject_always).
pub fn permission_options(params: &Value) -> Vec<(String, String, String)> {
    params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| Some((o["optionId"].as_str()?.to_string(), o["name"].as_str()?.to_string(), o["kind"].as_str().unwrap_or("").to_string())))
        .collect()
}

/// The tool call a permission request is about.
pub fn permission_tool_call(params: &Value) -> ToolCall {
    tool_call(&params["toolCall"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_updates() {
        assert_eq!(parse(&json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Hi" } })), Update::AgentText("Hi".into()));
        let call = parse(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit main.rs", "kind": "edit", "status": "pending",
            "content": [{ "type": "diff", "path": "/p/main.rs", "oldText": "a", "newText": "b" }],
            "locations": [{ "path": "/p/main.rs", "line": 3 }]
        }));
        let Update::ToolCall(c) = call else { panic!() };
        assert_eq!((c.id.as_str(), c.title.as_deref(), c.kind.as_deref(), c.status.as_deref()), ("t1", Some("Edit main.rs"), Some("edit"), Some("pending")));
        assert_eq!(c.content, Some(vec![ToolContent::Diff(Diff { path: "/p/main.rs".into(), old_text: Some("a".into()), new_text: "b".into() })]));
        assert_eq!(c.locations, Some(vec![("/p/main.rs".into(), Some(3))]));
        let Update::ToolCallUpdate(u) = parse(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" })) else { panic!() };
        assert_eq!((u.status.as_deref(), u.title, u.content), (Some("completed"), None, None));
        assert_eq!(
            parse(&json!({ "sessionUpdate": "plan", "entries": [{ "content": "Read", "priority": "high", "status": "completed" }] })),
            Update::Plan(vec![PlanEntry { content: "Read".into(), status: "completed".into() }])
        );
        let params = json!({ "options": [{ "optionId": "y", "name": "Allow", "kind": "allow_once" }], "toolCall": { "toolCallId": "t1" } });
        assert_eq!(permission_options(&params), [("y".to_string(), "Allow".to_string(), "allow_once".to_string())]);
        assert_eq!(permission_tool_call(&params).id, "t1");
    }
}
