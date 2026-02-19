use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub struct DSperseClient {
    socket_path: Option<String>,
}

impl DSperseClient {
    pub fn new(socket_path: Option<String>) -> Self {
        Self { socket_path }
    }

    pub async fn prove(
        &self,
        model_id: &str,
        inputs: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request = serde_json::json!({
            "method": "prove",
            "model_id": model_id,
            "inputs": inputs,
        });

        self.send_ipc(&request).await
    }

    pub async fn prove_slice(
        &self,
        circuit_id: &str,
        slice_num: &str,
        inputs: &serde_json::Value,
        outputs: Option<&serde_json::Value>,
        proof_system: &str,
    ) -> Result<serde_json::Value> {
        let request = serde_json::json!({
            "method": "prove_slice",
            "circuit_id": circuit_id,
            "slice_num": slice_num,
            "inputs": inputs,
            "outputs": outputs,
            "proof_system": proof_system,
        });

        self.send_ipc(&request).await
    }

    async fn send_ipc(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.send_ipc_inner(request),
        )
        .await
        .context("dsperse IPC timed out after 30s")?
    }

    async fn send_ipc_inner(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let socket_path = self.socket_path.as_deref().unwrap_or("/tmp/dsperse.sock");

        let mut stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connecting to dsperse at {socket_path}"))?;

        let payload = serde_json::to_vec(request)?;
        let len = u32::try_from(payload.len())
            .context("IPC payload exceeds u32::MAX")?
            .to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        anyhow::ensure!(
            resp_len <= 64 * 1024 * 1024,
            "IPC response length {resp_len} exceeds 64MB cap"
        );

        let mut resp_buf = vec![0u8; resp_len];
        stream.read_exact(&mut resp_buf).await?;

        let response: serde_json::Value = serde_json::from_slice(&resp_buf)?;
        Ok(response)
    }
}
