use std::collections::HashSet;
use std::future::Future;

use scraper::{Html, Node, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

/// Elements whose text is not visible content.
const NOT_VISIBLE: &[&str] = &["script", "style", "noscript", "template"];

/// A fetched page: the URL the browser ended on, its title and its HTML.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub html: String,
}

/// A hyperlink on a page, with its absolute URL and whitespace-collapsed text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Link {
    pub url: String,
    pub text: String,
}

/// Anything that can turn a URL into a [`Page`].
///
/// The stages in this crate depend on this trait, not on a browser, so they
/// run against a [`crate::browser::Browser`] in production and against fixed
/// pages in tests.
pub trait PageFetcher {
    fn fetch(&self, url: &str) -> impl Future<Output = Result<Page>> + Send;
}

impl Page {
    pub fn new(url: impl Into<String>, title: impl Into<String>, html: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: title.into(),
            html: html.into(),
        }
    }

    /// True when the page is an anti-bot interstitial (Cloudflare, DuckDuckGo,
    /// Google) instead of the content that was asked for. Such a page must
    /// not be judged as if it were content.
    pub fn is_challenge(&self) -> bool {
        const TITLES: &[&str] = &["just a moment..."];
        const MARKERS: &[&str] = &[
            "challenge-platform",
            "cf-chl",
            "bots use duckduckgo",
            "unusual traffic",
        ];
        let title = self.title.trim().to_ascii_lowercase();
        let html = self.html.to_ascii_lowercase();
        TITLES.contains(&title.as_str()) || MARKERS.iter().any(|m| html.contains(m))
    }

    /// Visible text of the document: scripts, styles and templates are left
    /// out and runs of whitespace are collapsed to single spaces.
    pub fn text(&self) -> String {
        let doc = Html::parse_document(&self.html);
        let mut raw = String::new();
        for node in doc.root_element().descendants() {
            let Node::Text(text) = node.value() else {
                continue;
            };
            let hidden = node.ancestors().any(
                |a| matches!(a.value(), Node::Element(el) if NOT_VISIBLE.contains(&el.name())),
            );
            if !hidden {
                raw.push_str(&text.text);
                raw.push(' ');
            }
        }
        collapse_ws(&raw)
    }

    /// Every `<a href>` on the page, resolved against the page URL.
    pub fn links(&self) -> Vec<Link> {
        // `a[href]` is a constant selector and always parses.
        self.select_links("a[href]").unwrap_or_default()
    }

    /// Links matched by a CSS selector. Elements without `href` are skipped,
    /// relative hrefs are resolved against [`Page::url`], and only http(s)
    /// targets are kept. Duplicates are dropped, first occurrence wins.
    pub fn select_links(&self, css: &str) -> Result<Vec<Link>> {
        let selector = Selector::parse(css).map_err(|_| Error::Selector(css.to_string()))?;
        let base = Url::parse(&self.url).ok();
        let doc = Html::parse_document(&self.html);

        let mut seen = HashSet::new();
        let mut links = Vec::new();
        for el in doc.select(&selector) {
            let Some(href) = el.value().attr("href") else {
                continue;
            };
            let Some(url) = resolve(base.as_ref(), href) else {
                continue;
            };
            if !seen.insert(url.clone()) {
                continue;
            }
            let text = collapse_ws(&el.text().collect::<String>());
            links.push(Link { url, text });
        }
        Ok(links)
    }
}

/// Resolves `href` against `base` and returns it as an absolute http(s) URL
/// without its fragment. Anything else (`mailto:`, `javascript:`, garbage) is `None`.
pub fn resolve(base: Option<&Url>, href: &str) -> Option<String> {
    let href = href.trim();
    let mut url = match base {
        Some(base) => base.join(href).ok()?,
        None => Url::parse(href).ok()?,
    };
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><head><title>Part X</title>
        <style>.a { color: red }</style><script>var secret = "nope";</script></head>
        <body><h1>Part X   datasheet</h1><noscript>enable js</noscript>
        <a href="/docs/x.pdf#top">Spec  sheet</a>
        <a href="https://other.example/d.zip">Driver</a>
        <a href="mailto:a@b.c">mail</a>
        <a href="/docs/x.pdf">Spec sheet again</a>
        <a>no href</a></body></html>"#;

    fn page() -> Page {
        Page::new("https://vendor.example/products/x/", "Part X", PAGE)
    }

    #[test]
    fn text_skips_scripts_styles_and_collapses_whitespace() {
        let text = page().text();
        assert!(text.contains("Part X datasheet"), "{text}");
        assert!(!text.contains("secret"));
        assert!(!text.contains("color"));
        assert!(!text.contains("enable js"));
    }

    #[test]
    fn links_are_absolute_http_only_and_deduplicated() {
        let links = page().links();
        let urls: Vec<&str> = links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://vendor.example/docs/x.pdf",
                "https://other.example/d.zip"
            ]
        );
        assert_eq!(links[0].text, "Spec sheet");
    }

    #[test]
    fn select_links_uses_css_and_rejects_bad_selectors() {
        let links = page().select_links("a[href$='.zip']").unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].text, "Driver");
        assert!(matches!(
            page().select_links("a[[["),
            Err(Error::Selector(_))
        ));
    }

    #[test]
    fn detects_anti_bot_interstitials_but_not_normal_pages() {
        let cloudflare = Page::new("https://d.example/", "Just a moment...", "<html></html>");
        assert!(cloudflare.is_challenge());
        let ddg = Page::new(
            "https://html.duckduckgo.com/html/",
            "DuckDuckGo",
            "<p>Unfortunately, bots use DuckDuckGo too.</p>",
        );
        assert!(ddg.is_challenge());
        assert!(!page().is_challenge());
    }

    #[test]
    fn resolve_handles_protocol_relative_and_rejects_schemes() {
        let base = Url::parse("https://a.example/x/").unwrap();
        assert_eq!(
            resolve(Some(&base), "//b.example/f").as_deref(),
            Some("https://b.example/f")
        );
        assert_eq!(resolve(Some(&base), "javascript:void(0)"), None);
        assert_eq!(resolve(None, "relative.html"), None);
    }
}
