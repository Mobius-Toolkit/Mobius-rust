use std::error::Error;

use mobius_domain::{TranscriptLine, TranscriptRow};
use serde_json::Value;

use crate::Engine;

const SHORT_LINES: usize = 3;
const SHORT_CHARS: usize = 300;
const MOBIUS_PREFIX: &str = "mcp__mobius__";

pub async fn lines(
    engine: &Engine,
    session: i64,
) -> Result<Vec<TranscriptLine>, Box<dyn Error + Send + Sync>> {
    let rows = engine.store.transcript().list(session).await?;
    // The first prompt of each session starts with the Role prompt.
    let first_prompt = rows
        .iter()
        .find(|row| row.kind == "prompt")
        .map(|row| row.id);
    Ok(rows
        .into_iter()
        .map(|row| {
            let folded = Some(row.id) == first_prompt;
            line(row, folded)
        })
        .collect::<Result<_, _>>()?)
}

fn line(row: TranscriptRow, folded: bool) -> Result<TranscriptLine, serde_json::Error> {
    let json: Value = serde_json::from_str(&row.json)?;
    let mut line = TranscriptLine {
        id: row.id,
        time: row.time,
        kind: row.kind,
        text: String::new(),
        harness_tool_name: None,
        body: None,
        folded,
        error: false,
        raw: row.json,
    };
    match line.kind.as_str() {
        "prompt" | "check" => first_line(&mut line, json["text"].as_str().unwrap_or_default()),
        "mcp_call" => {
            line.text = format!("mobius · {}", json["tool"].as_str().unwrap_or_default());
            let outcome = json.get("error").unwrap_or(&json["result"]);
            line.body = Some(short(
                &format!(
                    "{} → {}",
                    json["arguments"],
                    outcome.as_str().unwrap_or_default()
                ),
                SHORT_LINES,
            ));
            line.error = json.get("error").is_some();
        }
        "error" => {
            first_line(&mut line, json["message"].as_str().unwrap_or_default());
            line.error = true;
        }
        _ => update(&mut line, &json["update"]),
    }
    Ok(line)
}

fn update(line: &mut TranscriptLine, update: &Value) {
    let kind = update["sessionUpdate"].as_str().unwrap_or_default();
    match kind {
        "agent_message_chunk" => chunk(line, "message", update),
        "agent_thought_chunk" => chunk(line, "thought", update),
        "tool_call" | "tool_call_update" => {
            let label = kind.replace('_', " ");
            line.text = match (mobius_tool(update), update["title"].as_str()) {
                (Some((tool, name)), _) => {
                    line.harness_tool_name = Some(name);
                    format!("mobius · {tool}")
                }
                (None, Some(title)) => format!("{label} · {}", short(title, 1)),
                (None, None) => label,
            };
            line.body = tool_output(update).map(|output| short(&output, SHORT_LINES));
        }
        _ => line.text = kind.to_string(),
    }
}

// Each Harness names a Mobius tool in other fields of its tool call updates.
fn mobius_tool(update: &Value) -> Option<(String, String)> {
    let meta = &update["_meta"];
    if meta["mcp"]["server"] == "mobius" {
        let tool = meta["mcp"]["tool"].as_str()?;
        let name = update["title"].as_str().unwrap_or(tool);
        return Some((tool.to_string(), name.to_string()));
    }
    [
        &update["title"],
        &update["name"],
        &meta["claudeCode"]["toolName"],
        &meta["cognition.ai/toolName"],
        &meta["cognition.ai/inferenceToolName"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .find_map(|name| {
        let tool = name.strip_prefix(MOBIUS_PREFIX)?;
        Some((tool.to_string(), name.to_string()))
    })
}

fn first_line(line: &mut TranscriptLine, text: &str) {
    line.text = short(text, 1);
    line.body = (line.text != text).then(|| text.to_string());
}

fn chunk(line: &mut TranscriptLine, text: &str, update: &Value) {
    line.text = text.to_string();
    line.body = update["content"]["text"]
        .as_str()
        .map(|text| short(text, SHORT_LINES));
}

fn tool_output(update: &Value) -> Option<String> {
    let texts: Vec<&str> = update["content"]
        .as_array()?
        .iter()
        .filter_map(|block| block["content"]["text"].as_str())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn short(text: &str, lines: usize) -> String {
    let text = text.trim_end();
    let mut short: String = text
        .lines()
        .take(lines)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(SHORT_CHARS)
        .collect();
    if short != text {
        short.push('…');
    }
    short
}
