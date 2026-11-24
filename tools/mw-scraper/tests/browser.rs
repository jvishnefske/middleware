//! End-to-end: a real Chromium, driven through chromedriver, against a small
//! site served from this test. Needs chromedriver and Chromium, so it is
//! ignored by default:
//!
//!     MW_SCRAPER_CHROMEDRIVER=/snap/bin/chromium.chromedriver \
//!         cargo test -p mw-scraper --test browser -- --ignored

use mw_scraper::evaluate::Criteria;
use mw_scraper::pipeline::{self, QuerySpec};
use mw_scraper::source::{SearchEngine, SourceSpec, UrlFilter};
use mw_scraper::{Browser, BrowserConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves a fixed site on 127.0.0.1 and returns its base URL.
async fn serve_site() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let site_base = base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let base = site_base.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body) = route(&base, &path);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    base
}

fn route(base: &str, path: &str) -> (&'static str, String) {
    match path {
        // A results page whose links go through a redirect, like DuckDuckGo's.
        p if p.starts_with("/search") => (
            "200 OK",
            format!(
                r#"<html><body>
                <a class="result" href="/l/?uddg={spec}">Part X spec</a>
                <a class="result" href="/forum/post">Part X forum</a>
                </body></html>"#,
                spec = urlencode(&format!("{base}/spec.html"))
            ),
        ),
        "/spec.html" => (
            "200 OK",
            r#"<html><head><title>Part X Product Specification</title>
               <script>document.title += ""</script></head>
               <body><h1>Part X</h1><p>Electrical specification and firmware notes.</p>
               <a href="/downloads/partx-driver.zip">Driver package</a></body></html>"#
                .to_string(),
        ),
        "/forum/post" => (
            "200 OK",
            "<html><title>Forum</title><body>Part X chat, no files.</body></html>".to_string(),
        ),
        _ => (
            "404 Not Found",
            "<html><title>404</title></html>".to_string(),
        ),
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn driver_config() -> BrowserConfig {
    BrowserConfig {
        chromedriver: std::env::var("MW_SCRAPER_CHROMEDRIVER")
            .unwrap_or_else(|_| "chromedriver".to_string()),
        // Containers and some sandboxes refuse Chromium's own sandbox.
        chromium_args: vec!["--no-sandbox".to_string()],
        ..BrowserConfig::default()
    }
}

#[tokio::test]
#[ignore = "needs chromedriver and Chromium; see the module docs"]
async fn search_then_evaluate_finds_the_spec_page() {
    let base = serve_site().await;
    let browser = Browser::start(&driver_config())
        .await
        .expect("start Chromium");

    let spec = QuerySpec {
        name: "part-x".into(),
        sources: vec![SourceSpec::Search {
            query: "Part X".into(),
            engine: SearchEngine {
                name: "local".into(),
                url_template: format!("{base}/search?q={{query}}"),
                result_selector: "a.result".into(),
                redirect_param: Some("uddg".into()),
                redirect_base64_prefix: None,
            },
        }],
        filter: UrlFilter {
            excludes: vec!["forum".into()],
            ..UrlFilter::default()
        },
        criteria: Criteria {
            require: vec!["part x".into()],
            any_of: vec!["specification".into()],
            any_links: vec![".zip".into()],
            min_score: 1.0,
            ..Criteria::default()
        },
        max_pages: 5,
    };

    let findings = pipeline::run(&browser, &spec, &spec.criteria).await;
    browser.quit().await.expect("quit");
    let findings = findings.expect("run");

    assert_eq!(
        findings.len(),
        1,
        "forum result is filtered out: {findings:?}"
    );
    let finding = &findings[0];
    assert_eq!(finding.candidate.url, format!("{base}/spec.html"));
    assert_eq!(
        finding.title.as_deref(),
        Some("Part X Product Specification")
    );
    let verdict = finding.verdict.as_ref().expect("verdict");
    assert!(verdict.accepted, "{verdict:?}");
    assert_eq!(verdict.score, 1.0);
}

#[tokio::test]
#[ignore = "needs chromedriver and Chromium; see the module docs"]
async fn failing_page_is_reported_and_the_run_continues() {
    let base = serve_site().await;
    let browser = Browser::start(&driver_config())
        .await
        .expect("start Chromium");

    // Port 1 refuses connections, so the navigation itself fails.
    let spec = QuerySpec {
        name: "mixed".into(),
        sources: vec![SourceSpec::Urls {
            urls: vec![
                "http://127.0.0.1:1/down".into(),
                format!("{base}/spec.html"),
            ],
        }],
        filter: UrlFilter::default(),
        criteria: Criteria {
            require: vec!["electrical".into()],
            ..Criteria::default()
        },
        max_pages: 10,
    };

    let findings = pipeline::run(&browser, &spec, &spec.criteria).await;
    browser.quit().await.expect("quit");
    let findings = findings.expect("run");

    assert_eq!(findings.len(), 2);
    assert!(
        findings[0].verdict.is_none() && findings[0].error.is_some(),
        "{:?}",
        findings[0]
    );
    assert!(
        findings[1].verdict.as_ref().is_some_and(|v| v.accepted),
        "{:?}",
        findings[1]
    );
}
