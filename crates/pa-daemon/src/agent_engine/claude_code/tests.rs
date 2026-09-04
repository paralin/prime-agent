use super::*;
use crate::agent_engine::{AgentEngineConfig, SupervisorLinkConfig};
use crate::engine::SessionEngine;
use pa_core::session_engine::agent_messaging::mailbox::runtime::DurableMailbox;

#[cfg(unix)]
#[tokio::test]
async fn native_claude_worker_retains_the_query_and_bills_only_completed_results() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("claude-mock");
    std::fs::write(&executable, r"#!/usr/bin/env python3
import json, sys
with open('claude-argv.json', 'w') as output:
    json.dump(sys.argv[1:], output)
count = 0
tools = ['mcp__prime__family_' + name for name in ['list', 'send', 'inbox', 'wait']]
def emit(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    if message['type'] == 'control_request':
        emit({'type':'control_response', 'response':{'subtype':'success','request_id':message['request_id'],'response':{}}})
    elif message['type'] == 'user':
        count += 1
        if count == 1:
            emit({'type':'system','subtype':'init','model':'sonnet','tools':tools,'claude_code_version':'mock','session_id':'00000000-0000-4000-8000-000000000001'})
        emit({'type':'assistant','message':{'content':[{'type':'text','text':'answer-' + str(count)}],'usage':{'input_tokens':999,'output_tokens':999}}})
        emit({'type':'result','subtype':'success','is_error':False,'result':'answer-' + str(count),'usage':{'input_tokens':7,'output_tokens':3},'total_cost_usd':0.25,'num_turns':1})
").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({"claudeCode":{"executable":executable}}).to_string(),
    )
    .unwrap();
    let session_file = dir.path().join("child.jsonl");
    let mut store =
        crate::session_store::SessionFile::create(&dir.path().to_string_lossy(), None, 1);
    store.set_path(session_file.clone());
    store.rewrite().unwrap();
    store.persist_entry("custom_message",json!({"customType":"claude_code_session","content":"","display":false,"details":{"sessionId":"00000000-0000-4000-8000-000000000001"}})).unwrap();
    let engine = Arc::new(
        AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().into(),
            agent_dir,
            provider: Some("claude-code".into()),
            model: Some("sonnet".into()),
            api_key: None,
            thinking: Some(pa_types::ai::ModelThinkingLevel::High),
            session_dir: None,
            session_file: Some(session_file),
            faux_script: None,
            supervisor_link: Some(SupervisorLinkConfig {
                socket_path: dir.path().join("absent.sock"),
                active_session_id: "child".into(),
                worker_token: "token".into(),
            }),
            telemetry_disabled: Some(true),
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap(),
    );
    let mailbox = Arc::new(DurableMailbox::new(
        "child".into(),
        vec![],
        Arc::new(|_| Ok(())),
    ));
    *engine.mailbox_provider.lock().unwrap() = Some(Arc::new(move || Ok(mailbox.clone())));
    assert_eq!(engine.resolve_model().unwrap().provider, "claude-code");
    for index in 1..=3 {
        if index == 3 {
            engine.configure_model(crate::engine::EngineModelSelection {
                provider: Some("claude-code".into()),
                model: Some("opus".into()),
                api_key: None,
                thinking: Some(pa_types::ai::ModelThinkingLevel::High),
            });
            *engine.session_file.lock().unwrap() = None;
        }
        let events = tokio::task::spawn_blocking({
            let engine = engine.clone();
            move || {
                let mut events = Vec::new();
                engine.run_prompt(
                    index,
                    PromptRequest {
                        message: format!("task-{index}"),
                        source: "rpc".into(),
                        agent_message_id: None,
                        images: vec![],
                        batch: vec![],
                        custom_message: None,
                    },
                    &|| false,
                    &mut |event| {
                        events.push(event);
                        true
                    },
                );
                events
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(events.last(), Some(EngineEvent::Done(Ok(())))),
            "{events:?}"
        );
        let messages: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::AssistantMessage(value) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(messages.len(), 1);
        let answer_index = if index == 3 { 1 } else { index };
        assert_eq!(
            messages[0]["content"][0]["text"],
            format!("answer-{answer_index}")
        );
        assert_eq!(messages[0]["usage"]["input"], 7);
        assert_eq!(messages[0]["usage"]["cost"]["total"], 0.25);
        assert_eq!(events.iter().filter(|event| matches!(event,EngineEvent::CustomMessage(row) if row["customType"] == "claude_code_session")).count(),usize::from(index != 2));
    }
    let args: Vec<String> =
        serde_json::from_slice(&std::fs::read(dir.path().join("claude-argv.json")).unwrap())
            .unwrap();
    assert!(args.contains(&"--resume=00000000-0000-4000-8000-000000000001".into()));
    assert!(args.windows(2).any(|pair| pair == ["--model", "opus"]));
    let runtime = engine
        .claude_query
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .runtime
        .clone();
    runtime
        .handle_event(
            pa_core::session_engine::claude_code::ClaudeCodeEvent::ToolProgress {
                tool_use_id: "native-tool".into(),
                tool_name: "Read".into(),
                elapsed_seconds: 1.0,
            },
        )
        .unwrap();
    assert_eq!(engine.runtime_tool_names(), ["Read"]);
    runtime.abort("cancelled".into());
    assert!(engine.runtime_tool_names().is_empty());
    engine.dispose_claude_query().await;
    assert!(engine.claude_query.lock().unwrap().is_none());
}
