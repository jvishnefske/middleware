# mw-scraper

Selenium (WebDriver) over Chromium, for finding firmware, driver downloads
and specification sheets on the open web. A library and a CLI, in the
`middleware` workspace.

The work is two stages:

1. **Find URLs.** Query a search engine, read the links of one page, or take
   a fixed list. Then filter the candidates by host, extension and text.
2. **Evaluate pages.** Load a URL in Chromium and judge it against criteria
   (required and forbidden terms, alternatives, a matching link).

Both stages are plain Rust pieces that combine: `UrlSource`, `UrlFilter`,
`Evaluator`, `Criteria`, `Composite`, all over the `PageFetcher` trait.
A workflow is a `QuerySpec`, which is JSON.

## Requirements

- Chromium, and a chromedriver with the same major version. On this machine:
  `/snap/bin/chromium` and `/snap/bin/chromium.chromedriver` (both 158).
- Containers usually need `--chromium-arg=--no-sandbox`.

## CLI

```sh
export MW_SCRAPER_CHROMEDRIVER=/snap/bin/chromium.chromedriver
cargo run -p mw-scraper -- --chromium-arg=--no-sandbox search "nRF52840 product specification pdf"
cargo run -p mw-scraper -- --chromium-arg=--no-sandbox urls --spec tools/mw-scraper/examples/spec_sheet.json
cargo run -p mw-scraper -- --chromium-arg=--no-sandbox evaluate --url https://vendor.example/x --criteria criteria.json
cargo run -p mw-scraper -- --chromium-arg=--no-sandbox run --spec tools/mw-scraper/examples/spec_sheet.json --accepted-only
```

Output is JSON lines on stdout; errors go to stderr.

Browser options: `--webdriver-url` (use a WebDriver server already running,
e.g. `chromedriver --port=9515`), `--chromedriver`, `--chromium`, `--headed`,
`--page-timeout`, and repeatable `--chromium-arg`. Each also has an
`MW_SCRAPER_*` environment variable.

## Query spec

```json
{
  "name": "nrf52840-product-specification",
  "sources": [{ "kind": "search", "query": "nRF52840 product specification pdf" }],
  "filter": { "excludes": ["forum"], "extensions": [] },
  "criteria": {
    "require": ["nrf52840"],
    "any_of": ["product specification", "datasheet"],
    "any_min": 1,
    "forbid": ["buy now"],
    "any_links": [".pdf"],
    "min_score": 0.66
  },
  "max_pages": 4
}
```

Sources:

- `{"kind":"search","query":...,"engine":{...}}`: results of a search. The
  default engine is Bing. An engine is a URL template with `{query}`, a CSS
  selector for result links, and an optional redirect parameter (Bing's `u`,
  base64url with the `a1` prefix; DuckDuckGo's `uddg`).
- `{"kind":"links","url":...,"selector":"a.download"}`: links on one page,
  such as a vendor's download index. The selector is optional.
- `{"kind":"urls","urls":[...]}`: fixed URLs.

Criteria rules: all `require` terms and at least `any_min` of `any_of` must be
present, no `forbid` term may be present, and one of `any_links` must match a
link. The score is the share of wanted items found, and a page is accepted
when no rule failed and the score reaches `min_score`. Matching is
case-insensitive over the page URL, title and visible text, and over link
URLs and texts. Each rejected page says why.

## Library

```rust
let browser = Browser::start(&BrowserConfig::default()).await?;
let candidates = find_urls(&browser, &spec.sources, &spec.filter).await?;   // stage 1
let (page, verdict) = evaluate_url(&browser, &url, &criteria).await?;       // stage 2
let findings = run(&browser, &spec, &spec.criteria).await?;                 // both
browser.quit().await?;
```

To judge pages differently, implement `Evaluator`, or combine evaluators with
`Composite { combine: Combine::All | Combine::Any, parts }`. To list URLs
differently, implement `UrlSource`. Any `PageFetcher` works in place of
`Browser`, which keeps the stages testable without a browser.

## Limits

- **Bot challenges.** Some sites serve a challenge to automated browsers
  (Cloudflare's "Just a moment...", DuckDuckGo's bot check, and nordicsemi.com
  and digikey.com in testing). Such a page is reported as `Blocked` and is
  never judged as content. This tool does not try to get past them.
- **PDFs.** Chromium renders a PDF in its viewer, so its text is not in the
  DOM. A PDF is judged by its URL and file name. Extracting its text needs a
  PDF library and is not done yet.
- **Search engines.** Results depend on the engine. DuckDuckGo's HTML page
  challenges headless Chromium, so it is not the default.

## Tests

```sh
cargo test -p mw-scraper                                  # unit tests, no browser
MW_SCRAPER_CHROMEDRIVER=/snap/bin/chromium.chromedriver \
  cargo test -p mw-scraper --test browser -- --ignored    # end to end, local site
```
