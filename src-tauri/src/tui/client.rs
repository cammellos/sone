//! Connection to soned: one socket, subscribed to events, requests tagged
//! with ids. Everything that arrives (replies, events, disconnect) comes out
//! of one channel so the UI loop can `select!` on it.

use std::path::Path;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub enum Incoming {
    Reply { id: u64, result: Result<Value, String> },
    Event { name: String, data: Value },
    Disconnected,
}

pub struct Client {
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    pub async fn connect(path: &Path) -> std::io::Result<(Self, mpsc::UnboundedReceiver<Incoming>)> {
        let stream = UnixStream::connect(path).await?;
        let (read, writer) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let msg = match v["event"].as_str() {
                    Some(name) => Incoming::Event { name: name.to_string(), data: v["data"].clone() },
                    None => Incoming::Reply {
                        id: v["id"].as_u64().unwrap_or(0),
                        result: if v["ok"] == json!(true) {
                            Ok(v["result"].clone())
                        } else {
                            Err(v["error"].as_str().unwrap_or("error").to_string())
                        },
                    },
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            let _ = tx.send(Incoming::Disconnected);
        });
        Ok((Self { writer, next_id: 1 }, rx))
    }

    /// Send a request; its reply arrives as `Incoming::Reply` with this id.
    pub async fn request(&mut self, cmd: &str, args: Value) -> std::io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "id": id, "cmd": cmd, "args": args });
        self.writer.write_all(format!("{line}\n").as_bytes()).await?;
        Ok(id)
    }
}
