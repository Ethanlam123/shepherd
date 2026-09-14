//! The unix-socket side of the hook protocol: shepherd-hook connects here,
//! sends one JSON request line, and waits for one JSON reply line. The
//! server is a dumb relay - all policy lives in the adapter.

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// Wire decisions sent back to the hook, as serde values so the schema is
/// owned by one place (the hook's translator).
pub fn decision_value(decision: &str) -> Value {
    serde_json::json!({ "decision": decision })
}

/// What a hook connection turns into, sent to the adapter.
pub enum HookIn {
    Permission {
        session_id: String,
        tool: String,
        input: Value,
        reply: oneshot::Sender<Value>,
    },
    Notification {
        session_id: String,
        message: String,
    },
    /// The hook process waiting on a permission died (Claude Code killed it
    /// at its own timeout): the card is stale, the tool already proceeded.
    Dropped {
        session_id: String,
    },
}

/// Bind the socket, removing a stale file first. The parent directory is
/// created 0700 so only the same user can reach the socket.
pub fn bind(path: &std::path::Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_file(path); // stale socket from a crashed instance
    UnixListener::bind(path)
}

/// Accept loop; one task per connection.
pub async fn serve(listener: UnixListener, tx: mpsc::UnboundedSender<HookIn>) {
    loop {
        match listener.accept().await {
            Ok((conn, _)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(conn, tx).await {
                        eprintln!("shepherd: hook connection error: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("shepherd: hook accept error: {e}");
                return;
            }
        }
    }
}

async fn handle_conn(conn: UnixStream, tx: mpsc::UnboundedSender<HookIn>) -> std::io::Result<()> {
    let (mut read_half, mut write_half) = conn.into_split();
    // one request line; the hook keeps its write side open while it waits,
    // so EOF on this reader means the hook process is gone
    let mut line = String::new();
    let mut reader = BufReader::new(&mut read_half);
    read_line(&mut reader, &mut line).await?;
    let Ok(req) = serde_json::from_str::<Value>(line.trim()) else {
        return Ok(()); // not ours: ignore quietly
    };
    let session_id = |req: &Value| {
        req.get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match req.get("type").and_then(Value::as_str) {
        Some("permission") => {
            let (reply_tx, reply_rx) = oneshot::channel();
            let tool = req
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = req.get("input").cloned().unwrap_or(Value::Null);
            let sid = session_id(&req);
            if tx
                .send(HookIn::Permission {
                    session_id: sid.clone(),
                    tool,
                    input,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Ok(());
            }
            // reply if the adapter answers, or notice the hook dying first
            let mut probe = [0u8; 1];
            tokio::select! {
                reply = reply_rx => {
                    if let Ok(v) = reply {
                        let mut out = v.to_string();
                        out.push('\n');
                        write_half.write_all(out.as_bytes()).await?;
                        write_half.shutdown().await?;
                    }
                }
                r = reader.read(&mut probe) => {
                    // any completion here (EOF or error) means the peer ended:
                    // the hook only reads while waiting, never writes again
                    let _ = r;
                    let _ = tx.send(HookIn::Dropped { session_id: sid });
                }
            }
            Ok(())
        }
        Some("notification") => {
            let message = req
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let _ = tx.send(HookIn::Notification {
                session_id: session_id(&req),
                message,
            });
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Read one newline-terminated request with a sanity timeout; the hook
/// always writes immediately, so this only guards against stuck strangers.
async fn read_line(
    reader: &mut BufReader<&mut tokio::net::unix::OwnedReadHalf>,
    line: &mut String,
) -> std::io::Result<()> {
    use tokio::io::AsyncBufReadExt;
    match tokio::time::timeout(std::time::Duration::from_secs(30), reader.read_line(line)).await {
        Ok(res) => res.map(|_| ()),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "hook request line timeout",
        )),
    }
}
