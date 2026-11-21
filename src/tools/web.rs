//! `web_extract` (fetch a page as text) and `web_search` (SearXNG or Brave).

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

const EXTRACT_MAX: usize = 40_000;

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent(concat!("Mozilla/5.0 (X11; Linux x86_64) Mercury/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

pub fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100).unwrap_or_else(|_| html.to_string())
}

pub async fn extract(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url.trim())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("only http(s) URLs can be fetched");
    }
    let resp = http().get(parsed.clone()).send().await?;
    let status = resp.status();
    let ct = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("HTTP {status} from {parsed}");
    }
    let text = if ct.contains("html") || body.trim_start().starts_with('<') { html_to_text(&body) } else { body };
    let text = crate::tools::terminal::truncate_middle(text.trim(), EXTRACT_MAX);
    Ok(format!("# {parsed}\n\n{text}"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub fn format_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results.".into();
    }
    results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn search_searxng(base: &str, query: &str, n: usize) -> Result<Vec<SearchResult>> {
    let url = format!("{}/search", base.trim_end_matches('/'));
    let v: Value = http().get(&url).query(&[("q", query), ("format", "json")]).send().await?.error_for_status()?.json().await?;
    Ok(v.get("results")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(n)
                .map(|r| SearchResult {
                    title: r.get("title").and_then(Value::as_str).unwrap_or("").into(),
                    url: r.get("url").and_then(Value::as_str).unwrap_or("").into(),
                    snippet: r.get("content").and_then(Value::as_str).unwrap_or("").into(),
                })
                .collect()
        })
        .unwrap_or_default())
}

pub async fn search_brave(key: &str, query: &str, n: usize) -> Result<Vec<SearchResult>> {
    let v: Value = http()
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .query(&[("q", query), ("count", &n.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v.pointer("/web/results")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(n)
                .map(|r| SearchResult {
                    title: r.get("title").and_then(Value::as_str).unwrap_or("").into(),
                    url: r.get("url").and_then(Value::as_str).unwrap_or("").into(),
                    snippet: html_to_text(r.get("description").and_then(Value::as_str).unwrap_or("")).trim().into(),
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_conversion() {
        let t = html_to_text("<html><body><h1>Title</h1><p>Hello <b>world</b></p><script>x()</script></body></html>");
        assert!(t.contains("Title") && t.contains("Hello"));
        assert!(!t.contains("x()"));
    }

    #[test]
    fn results_format() {
        assert_eq!(format_results(&[]), "No results.");
        let s = format_results(&[SearchResult { title: "T".into(), url: "http://u".into(), snippet: " s ".into() }]);
        assert_eq!(s, "1. T\n   http://u\n   s");
    }

    #[tokio::test]
    async fn rejects_non_http() {
        assert!(extract("file:///etc/passwd").await.is_err());
    }
}
