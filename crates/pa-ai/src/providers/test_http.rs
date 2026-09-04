use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct ScriptedServer {
    pub url: String,
    pub requests: Arc<Mutex<Vec<(String, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ScriptedServer {
    pub async fn new(script: Vec<(u16, String, u64)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            for (status, body, delay) in script {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0_u8; 1];
                while !bytes.ends_with(b"\r\n\r\n") {
                    if socket.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    bytes.push(byte[0]);
                }
                let head = String::from_utf8(bytes).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                captured.lock().unwrap().push((head, value));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                let response = format!("HTTP/1.1 {status} scripted\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
