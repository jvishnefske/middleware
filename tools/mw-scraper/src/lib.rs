//! Selenium over Chromium for finding firmware, driver downloads and
//! specification sheets on the open web.
//!
//! The work is two stages, each a composable piece:
//!
//! 1. **Find URLs.** A [`source::UrlSource`] (a search engine, the links on
//!    one page, or a fixed list; see [`source::SourceSpec`]) yields candidate
//!    URLs. [`source::UrlFilter`] keeps the ones worth visiting.
//!    [`pipeline::find_urls`] runs this stage.
//! 2. **Evaluate pages.** [`pipeline::evaluate_url`] loads one URL and an
//!    [`evaluate::Evaluator`] judges the page. [`evaluate::Criteria`] is the
//!    data-driven judge; [`evaluate::Composite`] combines judges.
//!
//! [`pipeline::run`] chains both stages over a [`pipeline::QuerySpec`].
//! The stages depend on [`page::PageFetcher`], not on a browser:
//! [`browser::Browser`] implements it over WebDriver, and any other fetcher
//! (a cache, a fixture) works the same way.

pub mod browser;
pub mod error;
pub mod evaluate;
pub mod page;
pub mod pipeline;
pub mod source;

pub use browser::{Browser, BrowserConfig};
pub use error::{Error, Result};
pub use evaluate::{Combine, Composite, Criteria, Evaluator, Verdict};
pub use page::{Link, Page, PageFetcher};
pub use pipeline::{Finding, QuerySpec};
pub use source::{Candidate, SearchEngine, SourceSpec, UrlFilter, UrlSource};
