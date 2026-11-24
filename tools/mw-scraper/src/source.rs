use std::collections::HashSet;
use std::future::Future;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use url::Url;
use url::form_urlencoded;

use crate::error::Result;
use crate::page::{PageFetcher, resolve};

/// A URL that stage 1 found and that is worth visiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub url: String,
    /// Link text or search-result title; may be empty.
    pub title: String,
    /// Where it came from, e.g. `search:duckduckgo` or `links:<page url>`.
    pub origin: String,
}

/// Stage 1: something that yields candidate URLs.
///
/// Implement it for anything that can list URLs: a search engine, one page's
/// links, a fixed list, or a combination of those (see [`SourceSpec`]).
pub trait UrlSource {
    fn candidates<F: PageFetcher + Sync>(
        &self,
        fetcher: &F,
    ) -> impl Future<Output = Result<Vec<Candidate>>> + Send;
}

/// A search engine as a results page plus a selector for its result links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchEngine {
    pub name: String,
    /// Results URL; `{query}` is replaced by the form-encoded query.
    pub url_template: String,
    /// CSS selector for the result links on that page.
    pub result_selector: String,
    /// Query parameter that carries the real target when results go through
    /// a redirect, e.g. `uddg` for DuckDuckGo, `u` for Bing.
    pub redirect_param: Option<String>,
    /// When the redirect parameter is base64url-encoded, the prefix in front
    /// of the encoding (Bing: `a1`). Unset: the parameter is the URL itself.
    #[serde(default)]
    pub redirect_base64_prefix: Option<String>,
}

impl SearchEngine {
    /// Bing's results page. Works with headless Chromium; result links go
    /// through a redirect that carries the target base64url-encoded.
    pub fn bing() -> Self {
        Self {
            name: "bing".to_string(),
            url_template: "https://www.bing.com/search?q={query}".to_string(),
            result_selector: "li.b_algo h2 a".to_string(),
            redirect_param: Some("u".to_string()),
            redirect_base64_prefix: Some("a1".to_string()),
        }
    }

    /// DuckDuckGo's HTML-only results page. Serves a bot challenge to
    /// automated browsers, so it is only useful where that does not happen.
    pub fn duckduckgo() -> Self {
        Self {
            name: "duckduckgo".to_string(),
            url_template: "https://html.duckduckgo.com/html/?q={query}".to_string(),
            result_selector: "a.result__a".to_string(),
            redirect_param: Some("uddg".to_string()),
            redirect_base64_prefix: None,
        }
    }

    pub fn results_url(&self, query: &str) -> String {
        let encoded: String = form_urlencoded::byte_serialize(query.as_bytes()).collect();
        self.url_template.replace("{query}", &encoded)
    }

    /// The real target of a redirect link, or `url` itself when it is not one.
    pub fn unwrap_redirect(&self, url: &str) -> String {
        self.redirect_target(url).unwrap_or_else(|| url.to_string())
    }

    fn redirect_target(&self, url: &str) -> Option<String> {
        let param = self.redirect_param.as_ref()?;
        let parsed = Url::parse(url).ok()?;
        let (_, value) = parsed.query_pairs().find(|(key, _)| key == param)?;
        let target = match &self.redirect_base64_prefix {
            Some(prefix) => {
                let encoded = value.strip_prefix(prefix.as_str())?.trim_end_matches('=');
                String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?
            }
            None => value.into_owned(),
        };
        resolve(None, &target)
    }
}

fn default_engine() -> SearchEngine {
    SearchEngine::bing()
}

/// A stage-1 source as it appears in a query spec (JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSpec {
    /// Results of a search query.
    Search {
        query: String,
        #[serde(default = "default_engine")]
        engine: SearchEngine,
    },
    /// Links on one page, such as a vendor's download index. `selector`
    /// narrows them with CSS; the default is every `<a href>`.
    Links {
        url: String,
        #[serde(default)]
        selector: Option<String>,
    },
    /// URLs given directly.
    Urls { urls: Vec<String> },
}

impl UrlSource for SourceSpec {
    async fn candidates<F: PageFetcher + Sync>(&self, fetcher: &F) -> Result<Vec<Candidate>> {
        match self {
            SourceSpec::Search { query, engine } => {
                let page = fetcher.fetch(&engine.results_url(query)).await?;
                let origin = format!("search:{}", engine.name);
                let links = page.select_links(&engine.result_selector)?;
                Ok(links
                    .into_iter()
                    .map(|link| Candidate {
                        url: engine.unwrap_redirect(&link.url),
                        title: link.text,
                        origin: origin.clone(),
                    })
                    .collect())
            }
            SourceSpec::Links { url, selector } => {
                let page = fetcher.fetch(url).await?;
                let css = selector.as_deref().unwrap_or("a[href]");
                let origin = format!("links:{}", page.url);
                Ok(page
                    .select_links(css)?
                    .into_iter()
                    .map(|link| Candidate {
                        url: link.url,
                        title: link.text,
                        origin: origin.clone(),
                    })
                    .collect())
            }
            SourceSpec::Urls { urls } => Ok(urls
                .iter()
                .filter_map(|u| resolve(None, u))
                .map(|url| Candidate {
                    url,
                    title: String::new(),
                    origin: "urls".to_string(),
                })
                .collect()),
        }
    }
}

/// Narrows candidates by where they point. All conditions must hold; an
/// empty list means "no condition". Matching is case-insensitive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UrlFilter {
    /// Host must be one of these or a subdomain of one, e.g. `ti.com`.
    pub hosts: Vec<String>,
    /// Hosts (and their subdomains) that are never accepted.
    pub deny_hosts: Vec<String>,
    /// URL must contain at least one of these.
    pub contains_any: Vec<String>,
    /// URL must contain none of these.
    pub excludes: Vec<String>,
    /// Path must end in one of these, e.g. `.zip`, `.pdf`.
    pub extensions: Vec<String>,
}

impl UrlFilter {
    pub fn accepts(&self, raw: &str) -> bool {
        let Ok(url) = Url::parse(raw) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return false;
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let lower = raw.to_ascii_lowercase();
        let path = url.path().to_ascii_lowercase();

        let host_ok = self.hosts.is_empty() || self.hosts.iter().any(|h| host_matches(&host, h));
        let host_denied = self.deny_hosts.iter().any(|h| host_matches(&host, h));
        let contains_ok = self.contains_any.is_empty()
            || self
                .contains_any
                .iter()
                .any(|s| lower.contains(&s.to_ascii_lowercase()));
        let excluded = self
            .excludes
            .iter()
            .any(|s| lower.contains(&s.to_ascii_lowercase()));
        let extension_ok = self.extensions.is_empty()
            || self
                .extensions
                .iter()
                .any(|e| path.ends_with(&e.to_ascii_lowercase()));

        host_ok && !host_denied && contains_ok && !excluded && extension_ok
    }

    pub fn apply(&self, candidates: Vec<Candidate>) -> Vec<Candidate> {
        candidates
            .into_iter()
            .filter(|c| self.accepts(&c.url))
            .collect()
    }
}

fn host_matches(host: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_start_matches('.').to_ascii_lowercase();
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

/// Drops repeated URLs, keeping the first occurrence.
pub fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(c.url.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_is_form_encoded() {
        let url = SearchEngine::duckduckgo().results_url("nRF52 spec sheet&pdf");
        assert_eq!(
            url,
            "https://html.duckduckgo.com/html/?q=nRF52+spec+sheet%26pdf"
        );
    }

    #[test]
    fn plain_redirect_links_unwrap_to_their_target() {
        let engine = SearchEngine::duckduckgo();
        let wrapped =
            "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fvendor.example%2Fspec.pdf&rut=abc";
        assert_eq!(
            engine.unwrap_redirect(wrapped),
            "https://vendor.example/spec.pdf"
        );
        // Not a redirect: returned unchanged.
        assert_eq!(
            engine.unwrap_redirect("https://plain.example/"),
            "https://plain.example/"
        );
    }

    #[test]
    fn base64_redirect_links_unwrap_to_their_target() {
        // "https://vendor.example/spec.pdf" in base64url, prefixed with "a1" as Bing does.
        let target = URL_SAFE_NO_PAD.encode("https://vendor.example/spec.pdf");
        let wrapped = format!("https://www.bing.com/ck/a?!&&p=abc&ptn=3&u=a1{target}&ntb=1");
        assert_eq!(
            SearchEngine::bing().unwrap_redirect(&wrapped),
            "https://vendor.example/spec.pdf"
        );
        // A malformed payload leaves the link alone rather than dropping it.
        let broken = "https://www.bing.com/ck/a?u=a1!!!";
        assert_eq!(SearchEngine::bing().unwrap_redirect(broken), broken);
    }

    #[test]
    fn filter_checks_host_extension_and_text() {
        let filter = UrlFilter {
            hosts: vec!["vendor.example".into()],
            deny_hosts: vec!["spam.vendor.example".into()],
            contains_any: vec!["firmware".into()],
            excludes: vec!["forum".into()],
            extensions: vec![".zip".into()],
        };
        assert!(filter.accepts("https://dl.vendor.example/Firmware_1.2.zip"));
        assert!(!filter.accepts("https://spam.vendor.example/firmware.zip"));
        assert!(!filter.accepts("https://other.example/firmware.zip"));
        assert!(!filter.accepts("https://dl.vendor.example/firmware.pdf"));
        assert!(!filter.accepts("https://vendor.example/forum/firmware.zip"));
        assert!(!filter.accepts("ftp://vendor.example/firmware.zip"));
    }

    #[test]
    fn empty_filter_accepts_any_http_url() {
        let filter = UrlFilter::default();
        assert!(filter.accepts("http://x.example/"));
        assert!(!filter.accepts("not a url"));
    }

    #[test]
    fn dedupe_keeps_first_occurrence() {
        let c = |u: &str, origin: &str| Candidate {
            url: u.into(),
            title: String::new(),
            origin: origin.into(),
        };
        let out = dedupe(vec![c("a", "1"), c("b", "2"), c("a", "3")]);
        assert_eq!(out, vec![c("a", "1"), c("b", "2")]);
    }

    #[test]
    fn source_spec_json_round_trips_with_defaults() {
        let spec: SourceSpec =
            serde_json::from_str(r#"{"kind":"search","query":"x datasheet"}"#).unwrap();
        assert_eq!(
            spec,
            SourceSpec::Search {
                query: "x datasheet".into(),
                engine: SearchEngine::bing()
            }
        );
        let links: SourceSpec =
            serde_json::from_str(r#"{"kind":"links","url":"https://a.example/"}"#).unwrap();
        assert_eq!(
            links,
            SourceSpec::Links {
                url: "https://a.example/".into(),
                selector: None
            }
        );
    }
}
