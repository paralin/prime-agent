use std::io::Write;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde_json::Value;

const WIDTH: usize = 1280;
const HEIGHT: usize = 1280;
const COLUMNS: usize = 128;
const CAPACITY: usize = COLUMNS * 80;
const MAX_CELLS: usize = CAPACITY * 8;
// Public-domain X.org 5x8 BDF font, matching the caller-history renderer.
const FONT: &[u8] = include_bytes!("../../assets/history-font-5x8.bin");

#[derive(Debug, Default)]
pub struct HistorySnapshot {
    pub text: String,
    pub images: Vec<pa_agent::types::ImageContent>,
    pub message_count: usize,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct HistoryTextLayout {
    pub pages: Vec<String>,
    pub truncated: bool,
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.into();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(text: &str, maximum: usize) -> String {
    let units: Vec<_> = text.encode_utf16().collect();
    if units.len() <= maximum {
        return text.into();
    }
    let head = maximum / 4;
    let tail = maximum - head;
    format!(
        "{}\n[{} characters elided]\n{}",
        String::from_utf16_lossy(&units[..head]),
        units.len() - maximum,
        String::from_utf16_lossy(&units[units.len() - tail..])
    )
}

fn normalize(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    let mut spacing = false;
    let mut newlines = 0;
    while let Some(mut ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for escape in chars.by_ref() {
                if ('@'..='~').contains(&escape) {
                    break;
                }
            }
            continue;
        }
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            ch = '\n';
        }
        if matches!(ch, '\t' | '\u{c}' | '\u{b}' | ' ') {
            if !spacing {
                output.push(' ');
            }
            spacing = true;
            newlines = 0;
        } else {
            spacing = false;
            if ch == '\n' {
                newlines += 1;
                if newlines <= 2 {
                    output.push(ch);
                }
            } else {
                newlines = 0;
                output.push(ch);
            }
        }
    }
    output.trim().into()
}

#[must_use]
pub fn serialize_session_history(entries: &[Value]) -> String {
    let mut parts = Vec::new();
    for entry in entries {
        match entry["type"].as_str() {
            Some("message") => {
                let message = &entry["message"];
                match message["role"].as_str() {
                    Some("user") => {
                        let text = content_text(&message["content"]);
                        if !text.is_empty() {
                            parts.push(format!("USER\n{text}"));
                        }
                    }
                    Some("assistant") => {
                        if matches!(message["stopReason"].as_str(), Some("error" | "aborted")) {
                            continue;
                        }
                        let mut blocks = Vec::new();
                        for block in message["content"].as_array().into_iter().flatten() {
                            match block["type"].as_str() {
                                Some("text") => {
                                    if let Some(text) =
                                        block["text"].as_str().filter(|text| !text.is_empty())
                                    {
                                        blocks.push(text.into());
                                    }
                                }
                                Some("thinking") => {
                                    if let Some(text) =
                                        block["thinking"].as_str().filter(|text| !text.is_empty())
                                    {
                                        blocks.push(format!("THINK\n{text}"));
                                    }
                                }
                                Some("toolCall") => blocks.push(format!(
                                    "CALL {}\n{}",
                                    block["name"].as_str().unwrap_or_default(),
                                    truncate(&block["arguments"].to_string(), 4000)
                                )),
                                _ => {}
                            }
                        }
                        if !blocks.is_empty() {
                            parts.push(format!("ASSISTANT\n{}", blocks.join("\n")));
                        }
                    }
                    Some("toolResult") => {
                        let text = truncate(&content_text(&message["content"]), 8000);
                        if !text.is_empty() {
                            parts.push(format!(
                                "RESULT {}{}\n{text}",
                                message["toolName"].as_str().unwrap_or_default(),
                                if message["isError"] == true {
                                    " ERROR"
                                } else {
                                    ""
                                }
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Some("custom_message") => {
                let text = content_text(&entry["content"]);
                if !text.is_empty() {
                    parts.push(format!(
                        "CONTEXT {}\n{}",
                        entry["customType"].as_str().unwrap_or_default(),
                        truncate(&text, 8000)
                    ));
                }
            }
            Some("compaction" | "branch_summary") => {
                if let Some(text) = entry["summary"].as_str().filter(|text| !text.is_empty()) {
                    parts.push(format!("SUMMARY\n{}", truncate(text, 8000)));
                }
            }
            _ => {}
        }
    }
    normalize(&parts.join("\n\n"))
}

fn layout_cells(text: &str) -> Vec<u8> {
    let mut cells = Vec::new();
    let mut column = 0;
    for ch in text.chars() {
        if ch == '\n' {
            let padding = if column == 0 {
                COLUMNS
            } else {
                COLUMNS - column
            };
            cells.resize(cells.len() + padding, b' ');
            column = 0;
        } else {
            if column == COLUMNS {
                column = 0;
            }
            cells.push(if ch.is_ascii() && !ch.is_ascii_control() {
                ch as u8
            } else {
                b'?'
            });
            column += 1;
        }
    }
    cells
}

#[must_use]
pub fn layout_history_text(text: &str) -> HistoryTextLayout {
    let mut cells = layout_cells(text);
    let truncated = cells.len() > MAX_CELLS;
    if truncated {
        let marker = layout_cells(&format!(
            "[{} rendered cells elided]",
            cells.len() - MAX_CELLS
        ));
        let available = MAX_CELLS - marker.len();
        let head = available / 4;
        let tail = available - head;
        let mut bounded = cells[..head].to_vec();
        bounded.extend(marker);
        bounded.extend_from_slice(&cells[cells.len() - tail..]);
        cells = bounded;
    }
    HistoryTextLayout {
        pages: cells
            .chunks(CAPACITY)
            .map(|page| String::from_utf8_lossy(page).into_owned())
            .collect(),
        truncated,
    }
}

fn png_chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(&kind);
    output.extend_from_slice(data);
    let mut crc = u32::MAX;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    output.extend_from_slice(&(!crc).to_be_bytes());
}

fn render_page(text: &str) -> std::io::Result<Vec<u8>> {
    let mut scanlines = vec![255; (WIDTH + 1) * HEIGHT];
    for row in 0..HEIGHT {
        scanlines[row * (WIDTH + 1)] = 0;
    }
    for (index, byte) in text.bytes().take(CAPACITY).enumerate() {
        let glyph = usize::from(byte.saturating_sub(32).min(94));
        let cell_x = (index % COLUMNS) * 10;
        let cell_y = (index / COLUMNS) * 16;
        for row in 0..8 {
            let bits = FONT.get(glyph * 8 + row).copied().unwrap_or_default();
            for column in 0..5 {
                if bits & (0x80 >> column) == 0 {
                    continue;
                }
                for dy in 0..2 {
                    let at = (cell_y + row * 2 + dy) * (WIDTH + 1) + 1 + cell_x + column * 2;
                    scanlines[at] = 0;
                    scanlines[at + 1] = 0;
                }
            }
        }
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&scanlines)?;
    let compressed = encoder.finish()?;
    let mut output = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut header = Vec::new();
    header.extend_from_slice(&(WIDTH as u32).to_be_bytes());
    header.extend_from_slice(&(HEIGHT as u32).to_be_bytes());
    header.extend_from_slice(&[8, 0, 0, 0, 0]);
    png_chunk(&mut output, *b"IHDR", &header);
    png_chunk(&mut output, *b"IDAT", &compressed);
    png_chunk(&mut output, *b"IEND", &[]);
    Ok(output)
}

/// # Errors
/// Returns an error when PNG compression fails.
pub fn build_history_snapshot(
    text: &str,
    message_count: usize,
) -> std::io::Result<HistorySnapshot> {
    let normalized = normalize(text);
    let layout = layout_history_text(&normalized);
    let images = layout
        .pages
        .iter()
        .map(|page| {
            Ok(pa_agent::types::ImageContent {
                data: STANDARD.encode(render_page(page)?),
                mime_type: "image/png".into(),
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let text = normalized;
    Ok(HistorySnapshot {
        text,
        images,
        message_count,
        truncated: layout.truncated,
    })
}

/// # Errors
/// Returns an error when PNG compression fails.
pub fn build_session_history_snapshot(
    entries: &[Value],
    previous: Option<&HistorySnapshot>,
) -> std::io::Result<HistorySnapshot> {
    let current = serialize_session_history(entries);
    let text = [
        previous.map_or("", |snapshot| snapshot.text.as_str()),
        current.as_str(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let count = entries
        .iter()
        .filter(|entry| matches!(entry["type"].as_str(), Some("message" | "custom_message")))
        .count();
    build_history_snapshot(
        &text,
        previous.map_or(0, |snapshot| snapshot.message_count) + count,
    )
}

/// # Errors
/// Returns an error when PNG compression fails.
pub fn build_act_caller_history(
    entries: &[Value],
    current_tool_call_id: Option<&str>,
    previous_tool_call_id: Option<&str>,
) -> std::io::Result<HistorySnapshot> {
    let find = |id: &str, from: usize| {
        entries
            .iter()
            .enumerate()
            .skip(from)
            .find_map(|(index, entry)| {
                (entry["type"] == "message"
                    && entry["message"]["role"] == "assistant"
                    && entry["message"]["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|block| block["type"] == "toolCall" && block["id"] == id))
                .then_some(index)
            })
    };
    let Some(previous) = previous_tool_call_id.and_then(|id| find(id, 0)) else {
        return Ok(HistorySnapshot::default());
    };
    let current = current_tool_call_id
        .and_then(|id| find(id, previous + 1))
        .unwrap_or(entries.len());
    build_session_history_snapshot(&entries[previous + 1..current], None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_depth_call_boundaries_exclude_both_calls_and_encode_png() {
        let entries = vec![
            json!({"type":"message","message":{"role":"user","content":"before"}}),
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"previous","name":"ipython","arguments":{"code":"prior"}}]}}),
            json!({"type":"message","message":{"role":"user","content":"between"}}),
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"current","name":"ipython","arguments":{"code":"current"}}]}}),
        ];
        let snapshot =
            build_act_caller_history(&entries, Some("current"), Some("previous")).unwrap();
        assert_eq!(snapshot.text, "USER\nbetween");
        assert_eq!(snapshot.message_count, 1);
        let png = STANDARD.decode(&snapshot.images[0].data).unwrap();
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        assert_eq!(&png[16..24], &[0, 0, 5, 0, 0, 0, 5, 0]);
        assert!(build_act_caller_history(&entries, None, None)
            .unwrap()
            .images
            .is_empty());
    }

    #[test]
    fn bounded_layout_keeps_chronological_tail_and_handles_exact_width_newlines() {
        let layout = layout_history_text(&format!("{}TAIL_TOKEN", "line\n".repeat(1000)));
        assert!(layout.truncated);
        assert_eq!(layout.pages.len(), 8);
        assert!(layout.pages.last().unwrap().contains("TAIL_TOKEN"));
        let layout = layout_history_text(&format!("{}\nY", "x".repeat(128)));
        assert_eq!(layout.pages.join("").find('Y'), Some(128));
        let snapshot = build_history_snapshot(&"x".repeat(100_000), 1).unwrap();
        assert!(snapshot.truncated);
        assert!(snapshot.text.contains("characters elided"));
        assert_eq!(snapshot.images.len(), 8);
    }

    #[test]
    fn serialization_preserves_thinking_tools_summaries_and_normalizes_terminal_text() {
        let text = serialize_session_history(&[
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"reason"},{"type":"toolCall","name":"ipython","arguments":{"code":"1"}}]}}),
            json!({"type":"message","message":{"role":"toolResult","toolName":"ipython","isError":true,"content":[{"type":"text","text":"\u{1b}[31mfailed\u{1b}[0m\r\n  detail"}]}}),
            json!({"type":"compaction","summary":"summary"}),
        ]);
        assert!(text.contains("THINK\nreason\nCALL ipython"));
        assert!(text.contains("RESULT ipython ERROR\nfailed\n detail"));
        assert!(text.ends_with("SUMMARY\nsummary"));
    }
}
