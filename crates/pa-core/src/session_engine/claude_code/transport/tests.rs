use super::super::runtime::RuntimeStatus;
use super::*;

#[tokio::test]
async fn incomplete_frames_survive_cancelled_read_and_oversized_frames_are_rejected() {
    let (reader, mut writer) = tokio::io::duplex(128);
    let mut reader = BufReader::new(reader);
    let mut partial = Vec::new();
    writer.write_all(b"{\"type\":").await.unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(1),
        read_frame(&mut reader, &mut partial)
    )
    .await
    .is_err());
    assert_eq!(partial, b"{\"type\":");
    writer.write_all(b"\"result\"}\n").await.unwrap();
    assert_eq!(
        read_frame(&mut reader, &mut partial)
            .await
            .unwrap()
            .unwrap()["type"],
        "result"
    );
    assert!(partial.is_empty());
    let huge = vec![b'x'; MAX_FRAME_BYTES + 1];
    let mut huge_reader = BufReader::new(huge.as_slice());
    assert!(read_frame(&mut huge_reader, &mut partial)
        .await
        .unwrap_err()
        .to_string()
        .contains("bound"));
}

#[tokio::test]
async fn spawn_failure_settles_runtime_and_denied_permissions_do_not_call_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(ClaudeCodeRuntime::new(
        "task".into(),
        "sonnet".into(),
        vec![],
    ));
    let options = QueryOptions {
        executable: dir.path().join("absent"),
        cwd: dir.path().into(),
        model: "sonnet".into(),
        resume_session_id: None,
        effort: None,
        append_system_prompt: None,
        tools: vec![],
        required_tools: vec![],
        mcp_handler: None,
    };
    assert!(start_query(runtime.clone(), options).is_err());
    assert_eq!(runtime.admission().await.status, RuntimeStatus::Error);
    assert_eq!(
        runtime.initial_completion().await.status,
        RuntimeStatus::Error
    );
    assert!(runtime.snapshot().closed);
    let denied = handle_control(
        json!({"subtype":"can_use_tool","tool_name":"Agent"}),
        None,
        AbortSignal::never(),
    )
    .await
    .unwrap();
    assert_eq!(denied["behavior"], "deny");
    let unknown = handle_control(
        json!({"subtype":"mcp_message","server_name":"foreign","message":{}}),
        None,
        AbortSignal::never(),
    )
    .await;
    assert!(unknown.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_initialization_mcp_and_follow_up_use_the_retained_stream() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("mock-claude");
    std::fs::write(&executable,r"#!/usr/bin/env python3
import json
import sys

def emit(message):
    print(json.dumps(message), flush=True)

with open('argv.json', 'w') as output:
    json.dump(sys.argv[1:], output)
initialize = json.loads(sys.stdin.readline())
assert initialize['request']['subtype'] == 'initialize'
assert initialize['request']['sdkMcpServers'] == ['prime']
emit({'type':'control_response','response':{'subtype':'success','request_id':initialize['request_id'],'response':{}}})
initialized = False
for line in sys.stdin:
    message = json.loads(line)
    assert message['type'] == 'user'
    assert message['origin']['kind'] == 'coordinator'
    if not initialized:
        emit({'type':'system','subtype':'init','model':'sonnet','session_id':'mock-session','tools':['Read'],'claude_code_version':'mock'})
        initialized = True
    emit({'type':'control_request','request_id':'mcp','request':{'subtype':'mcp_message','server_name':'prime','message':{'jsonrpc':'2.0','id':7,'method':'tools/list'}}})
    response = json.loads(sys.stdin.readline())
    assert response['response']['request_id'] == 'mcp'
    assert response['response']['response']['mcp_response']['id'] == 7
    emit({'type':'result','subtype':'success','is_error':False,'result':message['message']['content'][0]['text'],
          'usage':{'input_tokens':3,'output_tokens':2},'total_cost_usd':0.5,'num_turns':2})
").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let handler: McpHandler = Arc::new(move |request, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(
            async move { Ok(json!({"jsonrpc":"2.0","id":request["id"],"result":{"tools":[]}})) },
        )
    });
    let runtime = Arc::new(ClaudeCodeRuntime::new(
        "initial".into(),
        "sonnet".into(),
        vec!["Read".into()],
    ));
    let mut snapshots = runtime.subscribe();
    let task = start_query(
        runtime.clone(),
        QueryOptions {
            executable,
            cwd: dir.path().into(),
            model: "sonnet".into(),
            resume_session_id: Some("00000000-0000-4000-8000-000000000001".into()),
            effort: Some("high".into()),
            append_system_prompt: None,
            tools: vec!["Read".into()],
            required_tools: vec!["Read".into()],
            mcp_handler: Some(handler),
        },
    )
    .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), runtime.initial_completion())
        .await
        .unwrap();
    assert_eq!(first.status, RuntimeStatus::Done, "{first:?}");
    assert_eq!(first.answer_preview.as_deref(), Some("initial"));
    assert_eq!(first.usage.requests, 2);
    runtime.deliver("follow-up".into()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        snapshots.wait_for(|snapshot| snapshot.usage.requests == 4),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        runtime.snapshot().answer_preview.as_deref(),
        Some("follow-up")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    runtime.dispose();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let args: Vec<String> =
        serde_json::from_slice(&std::fs::read(dir.path().join("argv.json")).unwrap()).unwrap();
    assert!(args.contains(&"--setting-sources=".into()));
    assert!(args.contains(&"--strict-mcp-config".into()));
    assert!(args.contains(&"--resume=00000000-0000-4000-8000-000000000001".into()));
    assert!(args
        .windows(2)
        .any(|pair| pair == ["--permission-mode", "dontAsk"]));
    assert!(args.windows(2).any(|pair| pair == ["--tools", "Read"]));
    assert!(args
        .windows(2)
        .any(|pair| pair == ["--disallowedTools", "Agent,Task,SendMessage"]));
}

#[cfg(unix)]
#[tokio::test]
async fn aborting_transport_task_kills_the_spawned_process_group() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("mock-claude");
    std::fs::write(&executable, "#!/usr/bin/env python3\nimport os, time\nwith open('pid', 'w') as f:\n    f.write(str(os.getpid()))\ntime.sleep(60)\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = Arc::new(ClaudeCodeRuntime::new(
        "task".into(),
        "sonnet".into(),
        vec![],
    ));
    let task = start_query(
        runtime,
        QueryOptions {
            executable,
            cwd: dir.path().into(),
            model: "sonnet".into(),
            resume_session_id: None,
            effort: None,
            append_system_prompt: None,
            tools: vec![],
            required_tools: vec![],
            mcp_handler: None,
        },
    )
    .unwrap();
    let pid_path = dir.path().join("pid");
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled transport must release its process group");
}
