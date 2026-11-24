use std::future::Future;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thirtyfour::prelude::*;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

use crate::error::{Error, Result};
use crate::page::{Page, PageFetcher};

/// How to get a Chromium to drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// URL of a WebDriver server that is already running, e.g.
    /// `http://127.0.0.1:9515`. Unset: `chromedriver` is started for this
    /// session and stopped when the [`Browser`] is dropped.
    pub webdriver_url: Option<String>,
    /// The chromedriver executable, used when `webdriver_url` is unset.
    pub chromedriver: String,
    /// The Chromium binary to drive. Unset lets chromedriver find one.
    pub chromium: Option<String>,
    /// Run without a window.
    pub headless: bool,
    /// Seconds a navigation may take before it fails.
    pub page_load_timeout_secs: u64,
    pub user_agent: Option<String>,
    /// Extra Chromium switches, e.g. `--no-sandbox` in a container.
    pub chromium_args: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            webdriver_url: None,
            chromedriver: "chromedriver".to_string(),
            chromium: None,
            headless: true,
            page_load_timeout_secs: 30,
            user_agent: None,
            chromium_args: Vec::new(),
        }
    }
}

/// Id of the frame that Chromium's network-error interstitial renders in.
const CHROME_ERROR_MARKER: &str = "main-frame-error";

/// One Chromium session over WebDriver.
pub struct Browser {
    driver: WebDriver,
    // Held so that a chromedriver we started is killed with the session.
    _server: Option<Child>,
}

impl Browser {
    pub async fn start(config: &BrowserConfig) -> Result<Self> {
        let (server_url, server) = match &config.webdriver_url {
            Some(url) => (url.clone(), None),
            None => {
                let port = free_port()?;
                let child = Command::new(&config.chromedriver)
                    .arg(format!("--port={port}"))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()?;
                wait_for_port(port).await?;
                (format!("http://127.0.0.1:{port}"), Some(child))
            }
        };

        let mut caps = DesiredCapabilities::chrome();
        if config.headless {
            caps.set_headless()?;
        }
        if let Some(binary) = &config.chromium {
            caps.set_binary(binary)?;
        }
        if let Some(agent) = &config.user_agent {
            caps.add_arg(&format!("--user-agent={agent}"))?;
        }
        for arg in &config.chromium_args {
            caps.add_arg(arg)?;
        }

        let driver = WebDriver::new(server_url, caps).await?;
        driver
            .set_page_load_timeout(Duration::from_secs(config.page_load_timeout_secs))
            .await?;
        Ok(Self {
            driver,
            _server: server,
        })
    }

    /// Navigates to `url` and returns the page as the browser ended up with
    /// it (after redirects and scripts).
    pub async fn navigate(&self, url: &str) -> Result<Page> {
        self.driver.goto(url).await?;
        let current = self.driver.current_url().await?.to_string();
        let title = self.driver.title().await?;
        let html = self.driver.source().await?;
        // A refused connection or DNS failure leaves Chromium on its own
        // error page and WebDriver still reports the requested URL, so the
        // error has to be recognised from the page itself.
        if html.contains(CHROME_ERROR_MARKER) {
            return Err(Error::Navigation(url.to_string()));
        }
        let page = Page::new(current, title, html);
        if page.is_challenge() {
            return Err(Error::Blocked(url.to_string()));
        }
        Ok(page)
    }

    /// Ends the WebDriver session.
    pub async fn quit(self) -> Result<()> {
        self.driver.quit().await?;
        Ok(())
    }
}

impl PageFetcher for Browser {
    fn fetch(&self, url: &str) -> impl Future<Output = Result<Page>> + Send {
        let url = url.to_string();
        async move { self.navigate(&url).await }
    }
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// chromedriver takes a moment to start listening; poll for it for about five seconds.
async fn wait_for_port(port: u16) -> Result<()> {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::DriverNotReady(port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_to_headless_chromedriver_on_path() {
        let cfg: BrowserConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, BrowserConfig::default());
        assert!(cfg.headless);
        assert_eq!(cfg.chromedriver, "chromedriver");
    }
}
