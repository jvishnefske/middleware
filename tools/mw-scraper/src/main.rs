use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use mw_scraper::evaluate::Criteria;
use mw_scraper::pipeline::{self, QuerySpec};
use mw_scraper::source::{SearchEngine, SourceSpec, UrlFilter};
use mw_scraper::{Browser, BrowserConfig, Verdict};
use serde::Serialize;

/// Finds pages on the web with Selenium and Chromium.
///
/// Output is JSON lines on stdout; progress and errors go to stderr.
#[derive(Parser)]
#[command(name = "mw-scraper", version)]
struct Cli {
    #[command(flatten)]
    browser: BrowserArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct BrowserArgs {
    /// A WebDriver server already running, e.g. http://127.0.0.1:9515.
    /// Unset: chromedriver is started for this run.
    #[arg(long, env = "MW_SCRAPER_WEBDRIVER")]
    webdriver_url: Option<String>,

    /// The chromedriver executable (used when --webdriver-url is unset).
    #[arg(long, env = "MW_SCRAPER_CHROMEDRIVER", default_value = "chromedriver")]
    chromedriver: String,

    /// The Chromium binary to drive. Unset lets chromedriver find one.
    #[arg(long, env = "MW_SCRAPER_CHROMIUM")]
    chromium: Option<String>,

    /// Show the browser window instead of running headless.
    #[arg(long)]
    headed: bool,

    /// Seconds a navigation may take.
    #[arg(long, default_value_t = 30)]
    page_timeout: u64,

    /// Extra Chromium switch, repeatable, e.g. --chromium-arg=--no-sandbox.
    #[arg(long = "chromium-arg", allow_hyphen_values = true)]
    chromium_args: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Stage 1: query Bing and print the candidate URLs.
    Search {
        /// The search query.
        query: String,
        /// JSON file with a UrlFilter to narrow the results.
        #[arg(long)]
        filter: Option<PathBuf>,
    },

    /// Stage 1: print the candidate URLs of a query spec's sources.
    Urls {
        /// Query spec (JSON): sources and filter are used.
        #[arg(long)]
        spec: PathBuf,
    },

    /// Stage 2: load one URL and judge it against criteria (JSON).
    Evaluate {
        #[arg(long)]
        url: String,
        /// JSON file with the Criteria.
        #[arg(long)]
        criteria: PathBuf,
    },

    /// Both stages: find candidates from a query spec, then evaluate them.
    Run {
        /// Query spec (JSON).
        #[arg(long)]
        spec: PathBuf,
        /// Print only the accepted pages.
        #[arg(long)]
        accepted_only: bool,
    },
}

#[derive(Serialize)]
struct Evaluated<'a> {
    url: &'a str,
    title: &'a str,
    verdict: &'a Verdict,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()).into())
}

fn print_json<T: Serialize>(value: &T) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn browser_config(args: &BrowserArgs) -> BrowserConfig {
    BrowserConfig {
        webdriver_url: args.webdriver_url.clone(),
        chromedriver: args.chromedriver.clone(),
        chromium: args.chromium.clone(),
        headless: !args.headed,
        page_load_timeout_secs: args.page_timeout,
        user_agent: None,
        chromium_args: args.chromium_args.clone(),
    }
}

async fn dispatch(browser: &Browser, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Search { query, filter } => {
            let filter: UrlFilter = match filter {
                Some(path) => read_json(&path)?,
                None => UrlFilter::default(),
            };
            let sources = [SourceSpec::Search {
                query,
                engine: SearchEngine::bing(),
            }];
            for candidate in pipeline::find_urls(browser, &sources, &filter).await? {
                print_json(&candidate)?;
            }
        }
        Command::Urls { spec } => {
            let spec: QuerySpec = read_json(&spec)?;
            for candidate in pipeline::find_urls(browser, &spec.sources, &spec.filter).await? {
                print_json(&candidate)?;
            }
        }
        Command::Evaluate { url, criteria } => {
            let criteria: Criteria = read_json(&criteria)?;
            let (page, verdict) = pipeline::evaluate_url(browser, &url, &criteria).await?;
            print_json(&Evaluated {
                url: &page.url,
                title: &page.title,
                verdict: &verdict,
            })?;
        }
        Command::Run {
            spec,
            accepted_only,
        } => {
            let spec: QuerySpec = read_json(&spec)?;
            let findings = pipeline::run(browser, &spec, &spec.criteria).await?;
            for finding in findings {
                let accepted = finding.verdict.as_ref().is_some_and(|v| v.accepted);
                if accepted_only && !accepted {
                    continue;
                }
                print_json(&finding)?;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let config = browser_config(&cli.browser);

    let browser = match Browser::start(&config).await {
        Ok(browser) => browser,
        Err(err) => {
            eprintln!("mw-scraper: cannot start Chromium: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let result = dispatch(&browser, cli.command).await;
    let quit = browser.quit().await;
    match (result, quit) {
        (Err(err), _) => {
            eprintln!("mw-scraper: {err}");
            std::process::ExitCode::FAILURE
        }
        (Ok(()), Err(err)) => {
            eprintln!("mw-scraper: closing the session: {err}");
            std::process::ExitCode::FAILURE
        }
        (Ok(()), Ok(())) => std::process::ExitCode::SUCCESS,
    }
}
