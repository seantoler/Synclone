//! JSON-RPC 2.0 over a local stream: one JSON object per line.
//!
//! Unix: a socket in the state directory (mode 0600). Windows: a named pipe
//! `\\.\pipe\synclone-<user>`. A long call may send notifications
//! (`{"jsonrpc":"2.0","method":"event","params":...}`) before its result,
//! e.g. the sign-in URL while a drive is being added.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// A line from the server: either a notification or the response.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerLine {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// Send one request and read lines until its response arrives; each
/// notification on the way is passed to `on_event`.
pub async fn call_on<S>(
    stream: S,
    method: &str,
    params: Value,
    on_event: &mut dyn FnMut(&Value),
) -> anyhow::Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (r, mut w) = tokio::io::split(stream);
    let req = Request {
        jsonrpc: "2.0".into(),
        id: Value::from(1),
        method: method.into(),
        params,
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    let mut lines = BufReader::new(r).lines();
    while let Some(l) = lines.next_line().await? {
        let msg: ServerLine = serde_json::from_str(&l)?;
        if msg.method.as_deref() == Some("event") {
            if let Some(p) = &msg.params {
                on_event(p);
            }
            continue;
        }
        if let Some(e) = msg.error {
            anyhow::bail!("{}", e.message);
        }
        return Ok(msg.result.unwrap_or(Value::Null));
    }
    anyhow::bail!("the Synclone service closed the connection")
}

pub fn default_pipe_name() -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into());
    let user: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!(r"\\.\pipe\{}-{}", synclone_core::APP_ID, user)
}

/// Connect to the daemon at `addr` (socket path or pipe name) and call.
pub async fn call(addr: &str, method: &str, params: Value, on_event: &mut dyn FnMut(&Value)) -> anyhow::Result<Value> {
    #[cfg(unix)]
    {
        let s = tokio::net::UnixStream::connect(addr).await.map_err(|e| {
            anyhow::anyhow!("Synclone is not running (cannot reach {addr}: {e}). Start it with `synclonectl daemon`.")
        })?;
        call_on(s, method, params, on_event).await
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        // ERROR_PIPE_BUSY (231): every instance is taken for a moment; retry.
        let mut tries = 0;
        let s = loop {
            match ClientOptions::new().open(addr) {
                Ok(s) => break s,
                Err(e) if e.raw_os_error() == Some(231) && tries < 50 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => {
                    anyhow::bail!(
                        "Synclone is not running (cannot reach {addr}: {e}). Start it with `synclonectl daemon`."
                    )
                }
            }
        };
        call_on(s, method, params, on_event).await
    }
}
