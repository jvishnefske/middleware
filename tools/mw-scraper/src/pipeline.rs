use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::evaluate::{Criteria, Evaluator, Verdict};
use crate::page::{Page, PageFetcher};
use crate::source::{Candidate, SourceSpec, UrlFilter, UrlSource, dedupe};

/// A whole workflow as data: where to look, what to keep, what to accept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySpec {
    pub name: String,
    pub sources: Vec<SourceSpec>,
    #[serde(default)]
    pub filter: UrlFilter,
    #[serde(default)]
    pub criteria: Criteria,
    /// Most pages to visit in one run.
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
}

fn default_max_pages() -> usize {
    10
}

/// One visited candidate and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub candidate: Candidate,
    /// Title of the page as loaded, when it loaded.
    pub title: Option<String>,
    pub verdict: Option<Verdict>,
    /// Why the page could not be loaded or evaluated.
    pub error: Option<String>,
}

/// Stage 1: collects candidate URLs from every source, applies the filter
/// and drops duplicates. Order follows the sources, then each source's order.
pub async fn find_urls<F: PageFetcher + Sync>(
    fetcher: &F,
    sources: &[SourceSpec],
    filter: &UrlFilter,
) -> Result<Vec<Candidate>> {
    let mut all = Vec::new();
    for source in sources {
        all.extend(source.candidates(fetcher).await?);
    }
    Ok(dedupe(filter.apply(all)))
}

/// Stage 2: loads one URL and judges the page.
pub async fn evaluate_url<F: PageFetcher + Sync, E: Evaluator>(
    fetcher: &F,
    url: &str,
    evaluator: &E,
) -> Result<(Page, Verdict)> {
    let page = fetcher.fetch(url).await?;
    let verdict = evaluator.evaluate(&page);
    Ok((page, verdict))
}

/// Both stages: find candidates, then visit up to `max_pages` of them. A page
/// that fails to load is recorded in its [`Finding`] and does not stop the run.
pub async fn run<F: PageFetcher + Sync, E: Evaluator>(
    fetcher: &F,
    spec: &QuerySpec,
    evaluator: &E,
) -> Result<Vec<Finding>> {
    let candidates = find_urls(fetcher, &spec.sources, &spec.filter).await?;
    let mut findings = Vec::new();
    for candidate in candidates.into_iter().take(spec.max_pages) {
        let finding = match evaluate_url(fetcher, &candidate.url, evaluator).await {
            Ok((page, verdict)) => Finding {
                candidate,
                title: Some(page.title),
                verdict: Some(verdict),
                error: None,
            },
            Err(err) => Finding {
                candidate,
                title: None,
                verdict: None,
                error: Some(err.to_string()),
            },
        };
        findings.push(finding);
    }
    Ok(findings)
}
