//! The on-disk sample log and the writer that feeds it without blocking a session.

use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Datelike;
use chrono::Utc;
use tokio::sync::mpsc;

use super::pricing::PriceTable;
use super::sample::UsageSample;
use super::summary::UsagePeriod;
use super::summary::UsageSummary;

const USAGE_DIR: &str = "usage";

/// `$NORI_HOME/usage/`: one append-only JSONL file per UTC month.
#[derive(Debug, Clone)]
pub struct UsageStore {
    dir: PathBuf,
    prices: PriceTable,
}

impl UsageStore {
    pub fn new(nori_home: &Path) -> Self {
        Self {
            dir: nori_home.join(USAGE_DIR),
            prices: PriceTable::load(nori_home),
        }
    }

    fn month_file(&self, year: i32, month: u32) -> PathBuf {
        self.dir.join(format!("{year:04}-{month:02}.jsonl"))
    }

    /// Append one sample. Blocking: callers on a runtime go through a [`UsageWriter`].
    pub fn append(&self, sample: &UsageSample) -> std::io::Result<()> {
        let Some(time) = sample.time() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage sample has no valid timestamp",
            ));
        };
        std::fs::create_dir_all(&self.dir)?;
        let mut line = serde_json::to_string(sample).map_err(std::io::Error::other)?;
        line.push('\n');
        // One write per line on an O_APPEND file: concurrent sessions never interleave a record.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.month_file(time.year(), time.month()))?
            .write_all(line.as_bytes())
    }

    /// Every readable sample in `since..=until`, and how many lines could not be read. A line
    /// that does not parse (a torn write, a newer format) is skipped, never fatal.
    pub fn read_since(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> (Vec<UsageSample>, i64) {
        self.read_matching(since, until, None)
    }

    /// The rate-limit windows of every account as of `now`, from the last days of the log.
    /// Only the lines that carry windows are parsed, so this is cheap enough for a picker.
    pub fn limits(&self, now: DateTime<Utc>) -> Vec<super::limits::AccountLimits> {
        let (samples, _) = self.read_matching(
            now - chrono::Duration::days(super::build::LIMIT_LOOKBACK_DAYS),
            now,
            Some("\"rate_limits\""),
        );
        super::limits::pool(&samples, now)
    }

    fn read_matching(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        needle: Option<&str>,
    ) -> (Vec<UsageSample>, i64) {
        let mut samples = Vec::new();
        let mut skipped = 0;
        let (mut year, mut month) = (since.year(), since.month());
        while (year, month) <= (until.year(), until.month()) {
            if let Ok(file) = std::fs::File::open(self.month_file(year, month)) {
                for line in BufReader::new(file)
                    .lines()
                    .map_while(Result::ok)
                    .filter(|line| needle.is_none_or(|needle| line.contains(needle)))
                {
                    let Ok(sample) = serde_json::from_str::<UsageSample>(&line) else {
                        skipped += 1;
                        continue;
                    };
                    if sample
                        .time()
                        .is_some_and(|time| time >= since && time <= until)
                    {
                        samples.push(sample);
                    }
                }
            }
            (year, month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
        }
        (samples, skipped)
    }

    /// Summarize `period` ending at `now`. Reads and folds on a blocking thread.
    pub async fn summary(&self, period: UsagePeriod, now: DateTime<Utc>) -> UsageSummary {
        let store = self.clone();
        tokio::task::spawn_blocking(move || UsageSummary::build(&store, period, now))
            .await
            .unwrap_or_else(|_| UsageSummary::empty(period, now))
    }

    pub(super) fn prices(&self) -> &PriceTable {
        &self.prices
    }
}

/// Every sample recorded in this process, as it is recorded.
fn feed() -> &'static tokio::sync::broadcast::Sender<UsageSample> {
    static FEED: std::sync::OnceLock<tokio::sync::broadcast::Sender<UsageSample>> =
        std::sync::OnceLock::new();
    FEED.get_or_init(|| tokio::sync::broadcast::channel(1024).0)
}

/// Watch the samples this process records from now on.
pub fn subscribe_samples() -> tokio::sync::broadcast::Receiver<UsageSample> {
    feed().subscribe()
}

/// Hands samples to a task that appends them, so recording never waits on the disk.
#[derive(Debug, Clone)]
pub struct UsageWriter {
    tx: mpsc::UnboundedSender<UsageSample>,
}

impl UsageWriter {
    /// Start the appender. Must be called inside a Tokio runtime. It ends when every clone of
    /// the writer is dropped.
    pub fn spawn(store: UsageStore) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<UsageSample>();
        tokio::spawn(async move {
            while let Some(sample) = rx.recv().await {
                let store = store.clone();
                let result = tokio::task::spawn_blocking(move || store.append(&sample)).await;
                if let Ok(Err(error)) = result {
                    tracing::warn!(target: "nori_usage", %error, "could not record a usage sample");
                }
            }
        });
        Self { tx }
    }

    pub fn record(&self, sample: UsageSample) {
        // Whoever watches the process's usage (a program charging its budget) hears of it first.
        let _ = feed().send(sample.clone());
        let _ = self.tx.send(sample);
    }
}
