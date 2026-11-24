/// Everything `mw-scraper` can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("webdriver: {0}")]
    WebDriver(#[from] thirtyfour::error::WebDriverError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid CSS selector {0:?}")]
    Selector(String),

    #[error("navigation to {0} failed (Chromium showed its error page)")]
    Navigation(String),

    #[error("{0} is behind a bot challenge; the page was not evaluated")]
    Blocked(String),

    #[error("chromedriver did not accept connections on port {0}")]
    DriverNotReady(u16),
}

pub type Result<T> = std::result::Result<T, Error>;
