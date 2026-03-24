//! Write-Ahead Log for L1 Short-Term Memory
//!
//! 每次写入先落 WAL，再写 redb。崩溃后从 WAL 重放恢复。
//! WAL 格式：每条记录 = len(4bytes) + JSON(Entry)
//! 文件滚动：超过 max_size 则创建新文件，旧文件异步压缩

use crate::common::Entry;
use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use parking_lot::RwLock;
use tokio::sync::mpsc;

const WAL_MAGIC: &[u8; 4] = b"WAL1";
const MAX_WAL_SIZE: u64 = 10 * 1024 * 1024; // 10MB per file

pub struct Wal {
    dir: PathBuf,
    current_file: RwLock<Option<File>>,
    current_size: RwLock<u64>,
    write_tx: mpsc::Sender<WalCmd>,
}

enum WalCmd {
    Append(Entry, tokio::sync::oneshot::Sender<Result<()>>),
    Flush(tokio::sync::oneshot::Sender<Result<()>>),
    Close,
}

impl Wal {
    /// 创建或打开 WAL 目录
    pub fn new(wal_dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = wal_dir.into();
        fs::create_dir_all(&dir)?;

        // 找到最新的 WAL 文件
        let latest = Self::latest_wal_file(&dir)?;
        let (file, size) = match latest {
            Some(path) => {
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                let size = f.metadata()?.len();
                (Some(f), size)
            }
            None => (None, 0),
        };

        let (tx, mut rx) = mpsc::channel::<WalCmd>(1000);
        let dir_clone = dir.clone();

        // 后台写线程
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(Self::bg_writer(&mut rx, dir_clone));
        });

        Ok(Self {
            dir,
            current_file: RwLock::new(file),
            current_size: RwLock::new(size),
            write_tx: tx,
        })
    }

    async fn bg_writer(rx: &mut mpsc::Receiver<WalCmd>, dir: PathBuf) {
        let mut current_file: Option<File> = None;
        let mut current_size: u64 = 0;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                WalCmd::Append(entry, resp) => {
                    let result = Self::append_to_wal(
                        &mut current_file,
                        &mut current_size,
                        &dir,
                    )(entry);
                    let _ = resp.send(result);
                }
                WalCmd::Flush(resp) => {
                    if let Some(ref mut f) = current_file {
                        let result = f.flush().map_err(Into::into);
                        let _ = resp.send(result);
                    } else {
                        let _ = resp.send(Ok(()));
                    }
                }
                WalCmd::Close => {
                    if let Some(ref mut f) = current_file {
                        let _ = f.flush();
                    }
                    break;
                }
            }
        }
    }

    fn append_to_wal<'a>(
        current_file: &'a mut Option<File>,
        current_size: &'a mut u64,
        dir: &'a PathBuf,
    ) -> impl FnMut(Entry) -> Result<()> + 'a {
        move |entry: Entry| {
            let json = serde_json::to_string(&entry)?;
            let json_bytes = json.as_bytes();
            let len = json_bytes.len() as u32;

            // 滚动检查
            if *current_size >= MAX_WAL_SIZE || current_file.is_none() {
                // 关闭旧文件（异步清理）
                *current_file = None;
                *current_size = 0;

                // 创建新文件
                let seq = Self::next_wal_seq(dir);
                let path = dir.join(format!("wal_{:06}.wal", seq));
                let f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(true)
                    .open(&path)?;
                *current_file = Some(f);
            }

            let file = current_file.as_mut().unwrap();

            // 写入：magic + len + json
            file.write_all(WAL_MAGIC)?;
            file.write_all(&len.to_le_bytes())?;
            file.write_all(json_bytes)?;
            file.flush()?;

            *current_size += (4 + 4 + json_bytes.len()) as u64;
            Ok(())
        }
    }

    /// 追加一条记录
    pub async fn append(&self, entry: Entry) -> Result<()> {
        let (resp, rx) = tokio::sync::oneshot::channel();
        self.write_tx
            .send(WalCmd::Append(entry, resp))
            .await
            .map_err(|_| anyhow::anyhow!("WAL writer closed"))?;
        rx.await?
    }

    /// 刷盘
    pub async fn flush(&self) -> Result<()> {
        let (resp, rx) = tokio::sync::oneshot::channel();
        self.write_tx
            .send(WalCmd::Flush(resp))
            .await
            .map_err(|_| anyhow::anyhow!("WAL writer closed"))?;
        rx.await?
    }

    /// 从 WAL 重放，恢复 L1 数据
    pub fn replay(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        let dir = &self.dir;

        let files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(dir) => dir
                .filter_map(|r| r.ok())
                .filter(|e| e.path().extension().map(|s| s == "wal").unwrap_or(false))
                .map(|e| e.path())
                .collect(),
            Err(_) => Vec::new(),
        };

        let mut files: Vec<PathBuf> = files;
        files.sort();

        for path in files {
            let file = match File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut reader = BufReader::new(file);

            loop {
                // 读 magic
                let mut magic = [0u8; 4];
                if reader.read_exact(&mut magic).is_err() {
                    break;
                }
                if &magic != WAL_MAGIC {
                    break;
                }

                // 读 len
                let mut len_bytes = [0u8; 4];
                if reader.read_exact(&mut len_bytes).is_err() {
                    break;
                }
                let len = u32::from_le_bytes(len_bytes) as usize;

                // 读 JSON
                let mut json_buf = vec![0u8; len];
                if reader.read_exact(&mut json_buf).is_err() {
                    break;
                }

                if let Ok(entry) = serde_json::from_slice::<Entry>(&json_buf) {
                    entries.push(entry);
                }
            }
        }

        tracing::info!("WAL 重放完成，恢复了 {} 条记忆", entries.len());
        entries
    }

    fn latest_wal_file(dir: &PathBuf) -> Result<Option<PathBuf>> {
        let files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(dir) => dir
                .filter_map(|r| r.ok())
                .filter(|e| e.path().extension().map(|s| s == "wal").unwrap_or(false))
                .map(|e| e.path())
                .collect(),
            Err(_) => return Ok(None),
        };

        if files.is_empty() {
            return Ok(None);
        }

        let mut files: Vec<PathBuf> = files;
        files.sort();
        Ok(files.into_iter().last())
    }

    fn next_wal_seq(dir: &PathBuf) -> u32 {
        let files: Vec<u32> = match std::fs::read_dir(dir) {
            Ok(dir) => dir
                .filter_map(|r| r.ok())
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|s| s.strip_prefix("wal_"))
                        .and_then(|s| s.strip_suffix(".wal"))
                        .and_then(|s| s.parse().ok())
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        files.into_iter().max().unwrap_or(0) + 1
    }
}
