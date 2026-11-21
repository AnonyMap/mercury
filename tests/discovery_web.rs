//! Local discovery and the web tools against mock servers.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use mercury::discovery;
use mercury::tools::{self, web};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn finds_a_local_server_and_skips_closed_ports() {
    let mock = common::start().await;
    let port: u16 = mock.url.rsplit(':').next().unwrap().parse().unwrap();
    // Grab a port that's certainly closed.
    let closed = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() };
    let found = discovery::discover(
        &[IpAddr::V4(Ipv4Addr::LOCALHOST)],
        &[(port, "FakeLlama"), (closed, "Nothing")],
        Duration::from_millis(300),
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(found.len(), 1);
    let d = &found[0];
    assert_eq!(d.server, "FakeLlama");
    assert!(d.is_this_machine);
    assert_eq!(d.models, vec!["other-model", "test-model"]);
    assert_eq!(d.base_url, format!("http://127.0.0.1:{port}/v1"));
    let p = d.to_provider(&mercury::config::Config::default());
    assert!(p.is_local());
    assert_eq!(p.default_model, "other-model");
}

#[tokio::test]
async fn probe_rejects_non_openai_servers() {
    let mock = common::start().await;
    // /page serves HTML, not a model list.
    assert!(discovery::probe(&format!("{}/page", mock.url), "x", Duration::from_secs(2)).await.is_none());
}

#[tokio::test]
async fn web_extract_and_searxng_search() {
    let mock = common::start().await;
    *mock.state.html.lock().unwrap() =
        "<html><head><title>T</title><style>.x{}</style></head><body><h1>Mercury docs</h1><p>Install the AppImage.</p></body></html>"
            .into();
    let page = web::extract(&format!("{}/page", mock.url)).await.unwrap();
    assert!(page.contains("Mercury docs") && page.contains("Install the AppImage."));
    assert!(!page.contains("<p>"));

    let dir = tempfile::tempdir().unwrap();
    let cfg = mercury::config::ToolsConfig { searxng_url: Some(mock.url.clone()), ..Default::default() };
    let ctx = tools::ToolContext::new(
        dir.path().to_path_buf(),
        mercury::memory::MemoryStore::new(dir.path().join("m")),
        mercury::skills::SkillLibrary::new(dir.path().join("s"), vec![]),
        std::sync::Arc::new(mercury::session::SessionStore::in_memory().unwrap()),
        "x",
        cfg,
        dir.path(),
    );
    let out = tools::execute("web_search", &json!({"query": "rust appimage"}), &ctx, &CancellationToken::new()).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.starts_with("1. Result for rust appimage\n   https://example.org/a"));
}
