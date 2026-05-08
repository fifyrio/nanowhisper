use crate::settings::AppSettings;
use anyhow::{Context, Result};
use std::time::Duration;

pub fn client_for_settings(settings: &AppSettings) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(300));

    match settings.proxy_mode.as_str() {
        "disabled" => {
            builder = builder.no_proxy();
        }
        "custom" => {
            let proxy_url = settings.proxy_url.trim();
            if proxy_url.is_empty() {
                anyhow::bail!("Proxy URL is required when custom proxy is enabled");
            }
            validate_proxy_url(proxy_url)?;
            let proxy = reqwest::Proxy::all(proxy_url).context("Invalid proxy URL")?;
            builder = builder.proxy(proxy);
        }
        _ => {}
    }

    builder.build().context("Failed to initialize HTTP client")
}

fn validate_proxy_url(proxy_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(proxy_url).context("Invalid proxy URL")?;
    match url.scheme() {
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h" => Ok(()),
        scheme => anyhow::bail!("Unsupported proxy scheme: {}", scheme),
    }
}
