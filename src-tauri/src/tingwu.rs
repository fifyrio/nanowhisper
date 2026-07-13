//! Alibaba Tingwu (通义听悟) offline speech-to-text provider.
//!
//! Unlike the OpenAI/Gemini providers (single synchronous request with inline
//! audio), Tingwu is a three-stage asynchronous flow:
//!   1. Upload the WAV to OSS and mint a presigned GET URL (Tingwu fetches it).
//!   2. Create an offline transcription task (`PUT /openapi/tingwu/v2/tasks`).
//!   3. Poll `GET /openapi/tingwu/v2/tasks/{id}` until COMPLETED, then download
//!      and parse the result JSON.
//!
//! Auth differs too: OSS uses the classic `OSS ak:sig` HMAC-SHA1 header, while
//! Tingwu uses the Alibaba Cloud API v3 signature (ACS3-HMAC-SHA256).

use anyhow::{bail, Context, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::settings::AppSettings;

const TINGWU_API_VERSION: &str = "2023-09-30";
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const MAX_POLL_ATTEMPTS: usize = 90; // 90 * 3s = 270s, under the 300s client timeout
const OSS_URL_TTL_SECS: u64 = 3600;

/// Resolved Tingwu credentials. Each field prefers the settings value and falls
/// back to the corresponding environment variable (loaded from `.env` in dev).
#[derive(Debug, Clone)]
pub struct TingwuConfig {
    pub access_key_id: String,
    pub access_key_secret: String,
    pub app_key: String,
    pub region: String,
    pub oss_endpoint: String,
    pub oss_bucket: String,
    pub oss_prefix: String,
}

fn pick(setting: &str, env_key: &str) -> String {
    let s = setting.trim();
    if !s.is_empty() {
        return s.to_string();
    }
    std::env::var(env_key).unwrap_or_default().trim().to_string()
}

impl TingwuConfig {
    /// Merge settings with env fallback and validate that required fields exist.
    pub fn resolve(settings: &AppSettings) -> Result<Self> {
        let mut region = pick(&settings.tingwu_region, "TINGWU_REGION");
        if region.is_empty() {
            region = "cn-beijing".to_string();
        }
        let mut oss_prefix = pick(&settings.tingwu_oss_prefix, "OSS_UPLOAD_PREFIX");
        // Normalize prefix: no leading slash, exactly one trailing slash (or empty).
        oss_prefix = oss_prefix.trim_start_matches('/').to_string();
        if !oss_prefix.is_empty() && !oss_prefix.ends_with('/') {
            oss_prefix.push('/');
        }

        let cfg = Self {
            access_key_id: pick(&settings.tingwu_access_key_id, "ALIBABA_CLOUD_ACCESS_KEY_ID"),
            access_key_secret: pick(
                &settings.tingwu_access_key_secret,
                "ALIBABA_CLOUD_ACCESS_KEY_SECRET",
            ),
            app_key: pick(&settings.tingwu_app_key, "TINGWU_APP_KEY"),
            region,
            oss_endpoint: pick(&settings.tingwu_oss_endpoint, "OSS_ENDPOINT"),
            oss_bucket: pick(&settings.tingwu_oss_bucket, "OSS_BUCKET_NAME"),
            oss_prefix,
        };

        let missing: Vec<&str> = [
            ("AccessKeyId", &cfg.access_key_id),
            ("AccessKeySecret", &cfg.access_key_secret),
            ("AppKey", &cfg.app_key),
            ("OSS Endpoint", &cfg.oss_endpoint),
            ("OSS Bucket", &cfg.oss_bucket),
        ]
        .iter()
        .filter(|(_, v)| v.is_empty())
        .map(|(name, _)| *name)
        .collect();

        if !missing.is_empty() {
            bail!("Tingwu not configured — missing: {}", missing.join(", "));
        }
        Ok(cfg)
    }

    fn tingwu_host(&self) -> String {
        format!("tingwu.{}.aliyuncs.com", self.region)
    }
}

// ── High-level orchestration ────────────────────────────────────────

/// Full pipeline: upload → create task → poll → fetch transcription text.
pub async fn transcribe_tingwu(
    client: &reqwest::Client,
    cfg: TingwuConfig,
    wav_data: Vec<u8>,
    language: Option<&str>,
) -> Result<String> {
    let object_key = format!("{}{}.wav", cfg.oss_prefix, uuid::Uuid::new_v4());
    upload_to_oss(client, &cfg, &object_key, wav_data)
        .await
        .context("Failed to upload audio to OSS")?;
    let file_url = presign_oss_get(&cfg, &object_key);

    let task_id = create_task(client, &cfg, &file_url, language)
        .await
        .context("Failed to create Tingwu task")?;

    let result_url = poll_task(client, &cfg, &task_id)
        .await
        .context("Tingwu transcription did not complete")?;

    fetch_transcription(client, &result_url)
        .await
        .context("Failed to parse Tingwu transcription result")
}

/// Lightweight validation: upload a tiny probe object. This exercises the OSS
/// signature and credentials (the most failure-prone part) without incurring a
/// full transcription task.
pub async fn validate_tingwu(client: &reqwest::Client, cfg: &TingwuConfig) -> Result<()> {
    let key = format!("{}nanowhisper-probe.txt", cfg.oss_prefix);
    upload_to_oss(client, cfg, &key, b"nanowhisper".to_vec())
        .await
        .context("OSS credential check failed")?;
    Ok(())
}

// ── OSS: upload + presigned GET (V1 HMAC-SHA1 signature) ─────────────

async fn upload_to_oss(
    client: &reqwest::Client,
    cfg: &TingwuConfig,
    object_key: &str,
    body: Vec<u8>,
) -> Result<()> {
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let content_type = "audio/wav";
    let resource = format!("/{}/{}", cfg.oss_bucket, object_key);
    let string_to_sign = format!("PUT\n\n{}\n{}\n{}", content_type, date, resource);
    let signature = hmac_sha1_base64(cfg.access_key_secret.as_bytes(), string_to_sign.as_bytes());

    let url = format!(
        "https://{}.{}/{}",
        cfg.oss_bucket, cfg.oss_endpoint, object_key
    );

    let resp = client
        .put(&url)
        .header("Date", &date)
        .header("Content-Type", content_type)
        .header(
            "Authorization",
            format!("OSS {}:{}", cfg.access_key_id, signature),
        )
        .body(body)
        .send()
        .await
        .context("OSS network error")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("OSS error {}: {}", status, text);
    }
    Ok(())
}

/// Build a presigned OSS GET URL so Tingwu can fetch the private object.
fn presign_oss_get(cfg: &TingwuConfig, object_key: &str) -> String {
    let expires = (chrono::Utc::now().timestamp() as u64) + OSS_URL_TTL_SECS;
    let resource = format!("/{}/{}", cfg.oss_bucket, object_key);
    let string_to_sign = format!("GET\n\n\n{}\n{}", expires, resource);
    let signature = hmac_sha1_base64(cfg.access_key_secret.as_bytes(), string_to_sign.as_bytes());

    format!(
        "https://{}.{}/{}?OSSAccessKeyId={}&Expires={}&Signature={}",
        cfg.oss_bucket,
        cfg.oss_endpoint,
        object_key,
        percent_encode(&cfg.access_key_id),
        expires,
        percent_encode(&signature),
    )
}

// ── Tingwu OpenAPI (ACS3-HMAC-SHA256 v3 signature) ───────────────────

async fn create_task(
    client: &reqwest::Client,
    cfg: &TingwuConfig,
    file_url: &str,
    language: Option<&str>,
) -> Result<String> {
    let body = serde_json::json!({
        "AppKey": cfg.app_key,
        "Input": {
            "SourceLanguage": source_language(language),
            "FileUrl": file_url,
            "TaskKey": format!("nanowhisper_{}", uuid::Uuid::new_v4().simple()),
        },
        "Parameters": {
            "Transcription": { "DiarizationEnabled": false }
        }
    });
    let body_bytes = serde_json::to_vec(&body)?;

    let resp = signed_request(
        client,
        cfg,
        "PUT",
        "/openapi/tingwu/v2/tasks",
        &[("type", "offline")],
        "CreateTask",
        Some(body_bytes),
    )
    .await?;

    let json = parse_json(resp).await?;
    json["Data"]["TaskId"]
        .as_str()
        .map(String::from)
        .context("Tingwu response missing Data.TaskId")
}

/// Poll until the task reaches a terminal state; return the transcription result URL.
async fn poll_task(client: &reqwest::Client, cfg: &TingwuConfig, task_id: &str) -> Result<String> {
    let path = format!("/openapi/tingwu/v2/tasks/{}", task_id);
    for _ in 0..MAX_POLL_ATTEMPTS {
        tokio::time::sleep(POLL_INTERVAL).await;

        let resp = signed_request(client, cfg, "GET", &path, &[], "GetTaskInfo", None).await?;
        let json = parse_json(resp).await?;

        let status = json["Data"]["TaskStatus"].as_str().unwrap_or_default();
        match status {
            "COMPLETED" => {
                return json["Data"]["Result"]["Transcription"]
                    .as_str()
                    .map(String::from)
                    .context("Completed task has no Transcription result URL");
            }
            "FAILED" => {
                let msg = json["Data"]["ErrorMessage"]
                    .as_str()
                    .or_else(|| json["Message"].as_str())
                    .unwrap_or("unknown error");
                bail!("Tingwu task failed: {}", msg);
            }
            _ => continue, // ONGOING / queued
        }
    }
    bail!("Tingwu task timed out after {} attempts", MAX_POLL_ATTEMPTS)
}

/// Download the result JSON and flatten it into a single transcript string.
async fn fetch_transcription(client: &reqwest::Client, result_url: &str) -> Result<String> {
    let resp = client
        .get(result_url)
        .send()
        .await
        .context("Failed to download transcription result")?;
    if !resp.status().is_success() {
        bail!("Result download error {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await.context("Result is not valid JSON")?;

    let paragraphs = json["Transcription"]["Paragraphs"]
        .as_array()
        .context("Result missing Transcription.Paragraphs")?;

    let text = paragraphs
        .iter()
        .filter_map(|p| p["Words"].as_array())
        .map(|words| {
            words
                .iter()
                .filter_map(|w| w["Text"].as_str())
                .collect::<String>()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if text.trim().is_empty() {
        bail!("Tingwu returned an empty transcription");
    }
    Ok(text.trim().to_string())
}

/// Sign and send an ACS3-HMAC-SHA256 request to the Tingwu OpenAPI.
async fn signed_request(
    client: &reqwest::Client,
    cfg: &TingwuConfig,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    action: &str,
    body: Option<Vec<u8>>,
) -> Result<reqwest::Response> {
    let host = cfg.tingwu_host();
    let date = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let body_bytes = body.unwrap_or_default();
    let content_sha256 = sha256_hex(&body_bytes);

    // Canonical query string: percent-encode + sort by key.
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (percent_encode(k), percent_encode(v)))
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Canonical (and signed) headers — sorted, lowercase.
    let headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("host".into(), host.clone()),
        ("x-acs-action".into(), action.to_string()),
        ("x-acs-content-sha256".into(), content_sha256.clone()),
        ("x-acs-date".into(), date.clone()),
        ("x-acs-signature-nonce".into(), nonce.clone()),
        ("x-acs-version".into(), TINGWU_API_VERSION.to_string()),
    ];
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
        .collect::<String>();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_uri = canonical_uri_encode(path);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, canonical_uri, canonical_query, canonical_headers, signed_headers, content_sha256
    );
    let string_to_sign = format!("ACS3-HMAC-SHA256\n{}", sha256_hex(canonical_request.as_bytes()));
    let signature = hmac_sha256_hex(
        cfg.access_key_secret.as_bytes(),
        string_to_sign.as_bytes(),
    );
    let authorization = format!(
        "ACS3-HMAC-SHA256 Credential={},SignedHeaders={},Signature={}",
        cfg.access_key_id, signed_headers, signature
    );

    let mut url = format!("https://{}{}", host, path);
    if !canonical_query.is_empty() {
        url.push('?');
        url.push_str(&canonical_query);
    }

    let req = client
        .request(reqwest::Method::from_bytes(method.as_bytes())?, &url)
        .header("Content-Type", "application/json")
        .header("host", &host)
        .header("x-acs-action", action)
        .header("x-acs-content-sha256", &content_sha256)
        .header("x-acs-date", &date)
        .header("x-acs-signature-nonce", &nonce)
        .header("x-acs-version", TINGWU_API_VERSION)
        .header("Authorization", authorization)
        .body(body_bytes);

    req.send().await.context("Tingwu network error")
}

async fn parse_json(resp: reqwest::Response) -> Result<serde_json::Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Tingwu API error {}: {}", status, text);
    }
    serde_json::from_str(&text).context("Tingwu returned invalid JSON")
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Map the app's language code to a Tingwu SourceLanguage value.
/// `auto`/unknown default to `cn` (Chinese-first service).
fn source_language(language: Option<&str>) -> &'static str {
    match language {
        Some("en") => "en",
        Some("ja") => "ja",
        Some("ko") => "ko",
        Some("es") => "es",
        Some("fr") => "fr",
        Some("de") => "de",
        Some("zh-Hans") | Some("zh-Hant") | Some("zh") => "cn",
        _ => "cn",
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    hex::encode(mac.finalize().into_bytes())
}

fn hmac_sha1_base64(key: &[u8], msg: &[u8]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Percent-encode per RFC3986 (unreserved: A-Z a-z 0-9 - _ . ~).
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Encode a URI path per RFC3986 while preserving `/` separators.
fn canonical_uri_encode(path: &str) -> String {
    path.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_empty_is_known_constant() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn percent_encode_leaves_unreserved_and_escapes_others() {
        assert_eq!(percent_encode("aA0-_.~"), "aA0-_.~");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn canonical_uri_preserves_slashes() {
        assert_eq!(
            canonical_uri_encode("/openapi/tingwu/v2/tasks"),
            "/openapi/tingwu/v2/tasks"
        );
    }

    #[test]
    fn source_language_defaults_to_chinese() {
        assert_eq!(source_language(None), "cn");
        assert_eq!(source_language(Some("auto")), "cn");
        assert_eq!(source_language(Some("en")), "en");
        assert_eq!(source_language(Some("zh-Hans")), "cn");
    }

    /// Live end-to-end test against the real Tingwu + OSS APIs.
    /// Ignored by default (needs network + credentials). Run with:
    ///   TINGWU_TEST_WAV=/path/to.wav cargo test --lib -- --ignored --nocapture live_
    #[tokio::test]
    #[ignore]
    async fn live_roundtrip() {
        let _ = dotenvy::from_path("../.env");
        let wav_path = std::env::var("TINGWU_TEST_WAV").expect("set TINGWU_TEST_WAV");
        let wav = std::fs::read(&wav_path).expect("read wav");

        let cfg = TingwuConfig::resolve(&AppSettings::default()).expect("resolve config");
        eprintln!(
            "config: region={} bucket={} endpoint={} prefix={:?}",
            cfg.region, cfg.oss_bucket, cfg.oss_endpoint, cfg.oss_prefix
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap();

        match transcribe_tingwu(&client, cfg, wav, None).await {
            Ok(text) => eprintln!("\n=== TRANSCRIPTION ===\n{}\n=====================", text),
            Err(e) => panic!("transcribe failed: {:#}", e),
        }
    }
}
