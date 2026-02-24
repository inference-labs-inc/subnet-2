use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client;

pub struct MinerHttpResponse {
    pub status: u16,
    pub body_bytes: Vec<u8>,
    pub elapsed_secs: f64,
}

pub async fn query_miner_http(
    client: &Client,
    ip: &str,
    port: u16,
    url_path: &str,
    body_json: &str,
    headers: &HashMap<String, String>,
    timeout_secs: f64,
) -> Result<MinerHttpResponse> {
    let url = sn2_types::format_http_url(ip, port, url_path);

    let mut req_builder = client
        .post(&url)
        .timeout(std::time::Duration::from_secs_f64(timeout_secs))
        .header("content-type", "application/json")
        .body(body_json.to_string());

    for (k, v) in headers {
        req_builder = req_builder.header(k.as_str(), v.as_str());
    }

    let start = Instant::now();
    let response = req_builder.send().await.context("HTTP request failed")?;
    let status = response.status().as_u16();

    if !response.status().is_success() {
        let elapsed = start.elapsed().as_secs_f64();
        let body = response.text().await.unwrap_or_default();
        let body_preview = match body.char_indices().nth(500) {
            Some((idx, _)) => &body[..idx],
            None => &body,
        };
        anyhow::bail!("HTTP {status} from miner after {elapsed:.3}s: {body_preview}");
    }

    let body_bytes = response
        .bytes()
        .await
        .context("reading response body")?
        .to_vec();
    let elapsed = start.elapsed().as_secs_f64();

    Ok(MinerHttpResponse {
        status,
        body_bytes,
        elapsed_secs: elapsed,
    })
}
