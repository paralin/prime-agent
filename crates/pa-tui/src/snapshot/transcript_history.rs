use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub(crate) fn transcript_history(tree: &Value, live: &[Value]) -> Vec<Value> {
    let entries: HashMap<_, _> = tree["flatNodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| node["entry"]["id"].as_str().map(|id| (id, &node["entry"])))
        .collect();
    let mut branch = Vec::new();
    let mut visited = HashSet::new();
    let mut cursor = tree["leafId"].as_str();
    while let Some(entry) = cursor.and_then(|id| entries.get(id)) {
        if !visited.insert(entry["id"].as_str()) {
            break;
        }
        branch.push(*entry);
        cursor = entry["parentId"].as_str();
    }
    branch.reverse();
    if branch.is_empty() {
        return live.to_vec();
    }
    let checkpoints: HashMap<_, _> = branch
        .iter()
        .filter(|entry| entry["type"] == "compaction")
        .filter_map(|entry| {
            let scratch = &entry["details"]["scratchHandoff"];
            (scratch["version"] == 1)
                .then(|| {
                    Some((
                        entry["firstKeptEntryId"].as_str()?,
                        scratch["path"].as_str()?,
                    ))
                })
                .flatten()
        })
        .collect();
    let mut messages = Vec::new();
    let mut persisted = HashSet::new();
    for entry in branch {
        match entry["type"].as_str() {
            Some("message") => {
                let message = &entry["message"];
                persisted.insert((message["role"].to_string(), message["timestamp"].to_string()));
                if let Some(path) = entry["id"].as_str().and_then(|id| checkpoints.get(id)).filter(|_| message["role"] == "user") {
                    let text = super::message_text(message);
                    let content = text.find("<scratch-handoff-file ").and_then(|start| text[start..].find(">\n").map(|offset| start + offset + 2))
                        .zip(text.rfind("\n</scratch-handoff-file>")).filter(|(start,end)| start < end)
                        .map_or(text.as_str(), |(start,end)| &text[start..end]);
                    messages.push(json!({"role":"custom","customType":"scratch-handoff-read","content":content,"display":true,"details":{"path":path},"timestamp":message["timestamp"]}));
                } else { messages.push(message.clone()); }
            }
            Some("custom_message") => {
                let mut message = entry.clone();
                message["role"] = json!("custom");
                persisted.insert((message["role"].to_string(), message["timestamp"].to_string()));
                messages.push(message);
            }
            Some("compaction") if entry["summary"].as_str().is_some_and(|text| !text.is_empty()) => messages.push(json!({"role":"compactionSummary","summary":entry["summary"],"tokensBefore":entry["tokensBefore"],"timestamp":entry["timestamp"]})),
            Some("branch_summary") => messages.push(json!({"role":"branchSummary","summary":entry["summary"],"fromId":entry["fromId"],"timestamp":entry["timestamp"]})),
            Some(_) | None => {}
        }
    }
    messages.extend(
        live.iter()
            .filter(|message| {
                message["role"] != "compactionSummary"
                    && message["role"] != "branchSummary"
                    && !persisted.contains(&(
                        message["role"].to_string(),
                        message["timestamp"].to_string(),
                    ))
            })
            .cloned(),
    );
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_checkpoint_preserves_its_selected_branch_and_live_tail() {
        let original = json!({"role":"user","content":"active request","timestamp":1});
        let continuation = json!({"role":"user","content":"Earlier history\n<scratch-handoff-file path=\"agent/work.org\">\n* TODO next action\n</scratch-handoff-file>","timestamp":2});
        let live = json!({"role":"assistant","content":[{"type":"text","text":"continuing"}],"timestamp":4});
        let tree = json!({"leafId":"boundary","flatNodes":[
            {"entry":{"id":"first","parentId":null,"type":"message","message":original}},
            {"entry":{"id":"other","parentId":"first","type":"message","message":{"role":"user","content":"other branch","timestamp":10}}},
            {"entry":{"id":"checkpoint","parentId":"first","type":"message","message":continuation}},
            {"entry":{"id":"boundary","parentId":"checkpoint","type":"compaction","summary":"","firstKeptEntryId":"checkpoint","details":{"scratchHandoff":{"version":1,"path":"agent/work.org"}}}}
        ]});
        assert_eq!(
            transcript_history(&tree, &[continuation, live.clone()]),
            vec![
                original,
                json!({"role":"custom","customType":"scratch-handoff-read","content":"* TODO next action","display":true,"details":{"path":"agent/work.org"},"timestamp":2}),
                live
            ]
        );
    }
}
