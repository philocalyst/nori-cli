//! A URL read on an interval. An event only when what was extracted from the response changes.
//!
//! The first reading is the baseline and says nothing, unless it already matches the monitor's
//! `fail_on` or `until`, which the pipeline decides. An endpoint that stops answering is a failure
//! after three readings in a row, said once; answering again is said once too.

use std::time::Duration;

use tokio::sync::mpsc::Sender;

use super::Item;
use super::jsonpath;
use crate::backend::monitor::event::Ending;
use crate::backend::monitor::spec::Extract;

/// The most of a response that is read.
const MAX_BODY: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const FAILURES_BEFORE_SAYING: i64 = 3;

pub(super) fn check(url: &str) -> Result<(), String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("`url` is not a URL: {error}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(format!("`url` must be http or https, not {scheme}")),
    }
}

pub(super) async fn run(
    url: &str,
    interval_seconds: i64,
    extract: &Extract,
    tx: &Sender<Item>,
) -> Option<Ending> {
    let client = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("nori-monitor")
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return Some(Ending::Failed {
                reason: format!("no HTTP client: {error}"),
            });
        }
    };
    let interval = Duration::from_secs(interval_seconds.max(2).unsigned_abs());
    let mut last: Option<String> = None;
    let mut failures = 0;
    let mut said_down = false;
    let mut first = true;
    loop {
        let reading = read(&client, url, extract).await;
        let mut lines: Vec<(String, bool)> = Vec::new();
        match reading {
            Ok(value) => {
                failures = 0;
                if said_down {
                    said_down = false;
                    lines.push((format!("{url} answers again"), false));
                }
                if last.as_ref() != Some(&value) {
                    last = Some(value.clone());
                    if first {
                        // What is already so is no news: only a rule that asked for it speaks.
                        if tx.send(Item::Baseline { text: value }).await.is_err() {
                            return None;
                        }
                    } else {
                        lines.push((value, false));
                    }
                }
            }
            Err(error) => {
                failures += 1;
                if failures >= FAILURES_BEFORE_SAYING && !said_down {
                    said_down = true;
                    lines.push((format!("{url} is not answering: {error}"), true));
                }
            }
        }
        first = false;
        for (text, failure) in lines {
            if tx.send(Item::Line { text, failure }).await.is_err() {
                return None;
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn read(client: &reqwest::Client, url: &str, extract: &Extract) -> Result<String, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if matches!(extract, Extract::Status) {
        return Ok(format!("HTTP {}", status.as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY {
            break;
        }
    }
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let text = String::from_utf8_lossy(&body);
    match extract {
        Extract::Status => Ok(format!("HTTP {}", status.as_u16())),
        Extract::JsonPath(path) => {
            let document: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| format!("the response is not JSON: {error}"))?;
            let found = jsonpath::select(&document, path)?;
            if found.is_empty() {
                return Err(format!("`{path}` selects nothing in the response"));
            }
            Ok(found
                .into_iter()
                .map(jsonpath::render)
                .collect::<Vec<_>>()
                .join(", "))
        }
        Extract::Regex(pattern) => {
            let regex = regex::Regex::new(pattern)
                .map_err(|error| format!("`regex` is not valid: {error}"))?;
            let captures = regex
                .captures(&text)
                .ok_or_else(|| format!("`{pattern}` matches nothing in the response"))?;
            Ok(captures
                .get(1)
                .or_else(|| captures.get(0))
                .map(|found| found.as_str().to_owned())
                .unwrap_or_default())
        }
    }
}
