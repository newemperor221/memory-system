//! L3 Archive Memory layer
//!
//! Long-term storage in daily `.md` files. Not involved in automatic recall.
//! On startup, optionally re-imports all archives back into L1.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};

use crate::common::{Entry, Importance, Layer};
use crate::l1::L1;

// ---------------------------------------------------------------------------
// L3 struct
// ---------------------------------------------------------------------------

/// Archive Memory layer.
///
/// Stores memories older than `archive_after_days` as daily `.md` files.
/// Not involved in automatic recall — only suggested after L0/L1/L2 all miss.
pub struct L3 {
    /// Directory where `.md` archive files are stored.
    archive_dir: PathBuf,
    /// Reference to L1 (source of truth for re-import).
    l1: Arc<L1>,
    /// Today's open archive file handle (appended to throughout the day).
    today_file: Option<File>,
    /// Today's date string `"YYYY-MM-DD"`.
    today_date: String,
    /// Stop signal sender for the background thread.
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
}

impl Drop for L3 {
    fn drop(&mut self) {
        // Ensure today's file is flushed on drop.
        if let Some(mut f) = self.today_file.take() {
            let _ = f.flush();
        }
    }
}

impl L3 {
    /// Create (or open) the archive directory and start background archival loop.
    pub fn new(l1: Arc<L1>, config: &crate::Config) -> Result<Self> {
        let archive_dir = PathBuf::from(&config.l3_archive_dir);
        fs::create_dir_all(&archive_dir)?;

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let today_path = archive_dir.join(format!("{}.md", today));
        let today_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&today_path)?;

        tracing::info!("L3 initialized at {}", archive_dir.display());

        let l3 = Self {
            archive_dir,
            l1,
            today_file: Some(today_file),
            today_date: today,
            stop_tx: None,
        };

        // Re-import all archives on startup (idempotent — re-writing existing keys replaces them).
        let imported = l3.import_all()?;
        if imported > 0 {
            tracing::info!("L3: recovered {} entries from archives", imported);
        }

        Ok(l3)
    }

    /// Write one entry to today's archive file (appended, not flushed per-call).
    pub fn archive(&mut self, entry: &Entry) -> Result<()> {
        let block = entry_to_markdown(entry);

        // Check if we've crossed into a new day.
        let today = Utc::now().format("%Y-%m-%d").to_string();
        if today != self.today_date {
            // Close old file and open new one.
            if let Some(mut f) = self.today_file.take() {
                let _ = f.flush();
                let _ = f.sync_all();
            }
            let path = self.archive_dir.join(format!("{}.md", today));
            self.today_file = Some(OpenOptions::new().create(true).append(true).open(&path)?);
            self.today_date = today;
        }

        // Now get mutable access to write
        if let Some(ref mut f) = self.today_file {
            std::io::Write::write_all(f, block.as_bytes())?;
            std::io::Write::flush(f)?;
        } else {
            return Err(anyhow!("archive file closed"));
        }
        Ok(())
    }

    /// Write multiple entries to today's archive.
    pub fn archive_batch(&mut self, entries: &[Entry]) -> Result<usize> {
        for entry in entries {
            self.archive(entry)?;
        }
        Ok(entries.len())
    }

    /// Return the path for an archive file of a given date.
    pub fn archive_path(&self, date: &str) -> PathBuf {
        self.archive_dir.join(format!("{}.md", date))
    }

    /// List all archive dates (filenames without `.md`).
    pub fn list_archives(&self) -> Vec<String> {
        let mut dates: Vec<String> = std::fs::read_dir(&self.archive_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                // e is Result<DirEntry> from ReadDir iterator
                let name = match e {
                    Ok(de) => de.file_name().to_str().unwrap_or_default().to_string(),
                    Err(_) => return None,
                };
                if name.ends_with(".md") {
                    Some(name.trim_end_matches(".md").to_string())
                } else {
                    None
                }
            })
            .collect();
        dates.sort();
        dates
    }

    /// Parse a single `---`-delimited block from a `.md` archive file.
    pub fn parse_entry(block: &str) -> Option<Entry> {
        let block = block.trim().trim_start_matches("---\n").trim_end_matches("\n---");
        let parts: Vec<&str> = block.splitn(2, "\n\n").collect();
        if parts.len() < 2 {
            return None;
        }
        let (meta, body) = (parts[0], parts[1]);

        let mut key = None;
        let mut importance = Importance::Normal;
        let mut source = "l3-archive".to_string();
        let mut tags: Vec<String> = Vec::new();

        for line in meta.lines() {
            let line = line.trim();
            if let Some(k) = line.strip_prefix("## ") {
                key = Some(k.trim().to_string());
            } else if let Some(v) = line.strip_prefix("**重要性**:") {
                let v = v.trim().to_lowercase();
                importance = Importance::from(v.as_str());
            } else if let Some(v) = line.strip_prefix("**来源**:") {
                source = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("**标签**:") {
                let v = v.trim().trim_start_matches('[').trim_end_matches(']');
                tags = v.split(',').map(|s| s.trim().to_string()).collect();
            }
        }

        let key = key?;
        let value = body.trim().to_string();
        let now = Utc::now();

        Some(Entry {
            id: uuid::Uuid::new_v4().to_string(),
            key,
            value,
            importance,
            tags,
            source,
            layer: Layer::Private,
            created_at: now,
            last_accessed: now,
            expires_at: None,
        })
    }

    /// Import all entries from one archive date back into L1.
    pub fn import_archive(&self, date: &str) -> Result<usize> {
        let path = self.archive_path(date);
        if !path.exists() {
            return Ok(0);
        }

        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut count = 0;

        let mut current_block = String::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim() == "---" {
                if !current_block.is_empty() {
                    if let Some(entry) = Self::parse_entry(&current_block) {
                        let key = entry.key.clone();
                        if self.l1.write(&key, &entry).is_ok() {
                            count += 1;
                        }
                    }
                    current_block.clear();
                }
            } else {
                current_block.push_str(&line);
                current_block.push('\n');
            }
        }

        tracing::info!("L3 import {}: {} entries from {}.md", date, count, date);
        Ok(count)
    }

    /// Import all archive files back into L1.
    pub fn import_all(&self) -> Result<usize> {
        let dates = self.list_archives();
        let mut total = 0;
        for date in dates {
            total += self.import_archive(&date)?;
        }
        Ok(total)
    }

    /// Start the background archival loop.
    /// Uses std::thread + std::sync::mpsc (compatible, no tokio needed).
    pub fn start_background_loop(mut self, interval_secs: u64) -> thread::JoinHandle<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.stop_tx = Some(tx);

        let archive_dir = self.archive_dir.clone();
        let l1 = self.l1.clone();

        thread::spawn(move || {
            let interval = Duration::from_secs(interval_secs);

            loop {
                // Check for stop signal with timeout
                match rx.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::info!("L3: received stop signal, exiting archival loop");
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Time to run the archival pass
                    }
                }

                // Scan L1 for entries older than archive_after_days
                let cutoff = Utc::now() - chrono::Duration::days(30);
                let cutoff_ts = cutoff.timestamp();
                let all_entries = l1.full_scan();
                let to_archive: Vec<_> = all_entries
                    .into_iter()
                    .filter(|e| e.created_at.timestamp() < cutoff_ts)
                    .collect();

                if to_archive.is_empty() {
                    continue;
                }

                tracing::info!("L3: archiving {} old entries", to_archive.len());

                // Write to today's archive
                let today = Utc::now().format("%Y-%m-%d").to_string();
                let path = archive_dir.join(format!("{}.md", today));
                let mut file = match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!("L3: failed to open archive file: {}", e);
                        continue;
                    }
                };

                for entry in &to_archive {
                    let block = entry_to_markdown(entry);
                    if let Err(e) = file.write_all(block.as_bytes()) {
                        tracing::error!("L3: failed to write entry: {}", e);
                    }
                    let _ = l1.delete(&entry.key);
                }

                if let Err(e) = file.flush() {
                    tracing::error!("L3: failed to flush archive file: {}", e);
                }
            }
        })
    }

    /// Stop the background archival loop.
    pub fn stop(&self) -> Result<()> {
        if let Some(tx) = &self.stop_tx {
            let _ = tx.send(());
        }
        Ok(())
    }

    /// Flush and close today's archive file.
    pub fn finalize_today(&mut self) -> Result<()> {
        if let Some(mut f) = self.today_file.take() {
            f.flush()?;
            // Sync to disk
            f.sync_all()?;
        }
        Ok(())
    }

    /// Number of archive files.
    pub fn count_archives(&self) -> usize {
        self.list_archives().len()
    }

    /// Check health.
    pub fn health_issue(&self) -> Option<String> {
        if !self.archive_dir.exists() {
            Some("archive directory missing".to_string())
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown serialisation
// ---------------------------------------------------------------------------

fn entry_to_markdown(entry: &Entry) -> String {
    let importance_str = match entry.importance {
        Importance::Low => "Low",
        Importance::Normal => "Normal",
        Importance::High => "High",
        Importance::Critical => "Critical",
    };
    let tags_str = entry.tags.join(", ");

    format!(
        "## {}\n**重要性**: {}\n**来源**: {}\n**标签**: [{}]\n**创建时间**: {}\n\n{}\n\n---\n",
        entry.key,
        importance_str,
        entry.source,
        tags_str,
        entry.created_at.to_rfc3339(),
        entry.value,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_roundtrip() {
        let entry = Entry::new(
            "private:test:001".into(),
            "hello world".into(),
            Importance::High,
            vec!["test".into(), "foo".into()],
            "agent-x".into(),
            Layer::Private,
        );

        let md = entry_to_markdown(&entry);
        let parsed = L3::parse_entry(&md).expect("should parse");

        assert_eq!(parsed.key, entry.key);
        assert_eq!(parsed.value, entry.value);
        assert_eq!(parsed.importance, Importance::High);
        assert_eq!(parsed.tags, entry.tags);
        assert_eq!(parsed.source, entry.source);
    }

    #[test]
    fn test_archive_and_import() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();

        // Set up a minimal config
        let l1_path = tmp.path().join("l1.db");
        let config = crate::Config {
            l1_path: l1_path.to_string_lossy().to_string(),
            l3_archive_dir: tmp.path().join("archive").to_string_lossy().to_string(),
            ..crate::Config::default()
        };

        let l1 = Arc::new(L1::new(&config).unwrap());
        let mut l3 = L3::new(l1.clone(), &config).unwrap();

        let entry = Entry::new(
            "private:test:arch".into(),
            "archived content".into(),
            Importance::Normal,
            vec![],
            "test".into(),
            Layer::Private,
        );

        // Archive it
        l3.archive(&entry).unwrap();

        // Check archive file exists
        let archives = l3.list_archives();
        assert!(!archives.is_empty());

        // Parse from archive
        let path = l3.archive_path(&archives[0]);
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed = L3::parse_entry(&content).expect("should parse");
        assert_eq!(parsed.value, "archived content");

        l3.finalize_today().unwrap();
    }

    #[test]
    fn test_count_archives() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        // Create some dummy .md files
        std::fs::write(tmp.path().join("2026-01-01.md"), "").unwrap();
        std::fs::write(tmp.path().join("2026-01-02.md"), "").unwrap();

        let l1_path = tmp.path().join("l1.db");
        let config = crate::Config {
            l1_path: l1_path.to_string_lossy().to_string(),
            l3_archive_dir: tmp.path().to_string_lossy().to_string(),
            ..crate::Config::default()
        };

        let l1 = Arc::new(L1::new(&config).unwrap());
        let l3 = L3::new(l1, &config).unwrap();
        assert_eq!(l3.count_archives(), 2);
    }
}
