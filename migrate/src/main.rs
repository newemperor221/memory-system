//! Migration tool: reads from v1 sled DB, writes to v2 SQLite


use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entry {
    pub id: String,
    pub key: String,
    pub value: String,
    pub importance: Importance,
    pub tags: Vec<String>,
    pub source: String,
    pub layer: Layer,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_accessed: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

// Importance: stored as i32 in DB, deserialized from lowercase string or number
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Importance(pub i32);

impl Importance {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "critical" => Self(4),
            "high" => Self(3),
            "normal" => Self(2),
            _ => Self(1),
        }
    }
    pub fn as_i32(&self) -> i32 { self.0 }
}

impl From<i32> for Importance {
    fn from(v: i32) -> Self { Self(v) }
}

impl serde::Serialize for Importance {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error> where S: serde::Serializer {
        s.serialize_i32(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Importance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: serde::Deserializer<'de> {
        use serde::de::Error;
        let val = serde_json::Value::deserialize(deserializer)?;
        match &val {
            serde_json::Value::String(v) => Ok(Self::from_str(v)),
            serde_json::Value::Number(v) => Ok(Self(v.as_i64().unwrap_or(2) as i32)),
            _ => Err(D::Error::custom(format!("Expected string or number for Importance, got {}", val))),
        }
    }
}

// Layer: stored as string "private"/"public"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layer(pub &'static str);

impl Layer {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "public" => Self("public"),
            _ => Self("private"),
        }
    }
    pub fn as_str(&self) -> &'static str { self.0 }
}

impl serde::Serialize for Layer {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error> where S: serde::Serializer {
        s.serialize_str(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Layer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: serde::Deserializer<'de> {
        use serde::de::Error;
        let val = serde_json::Value::deserialize(deserializer)?;
        match &val {
            serde_json::Value::String(v) => Ok(Self::from_str(v)),
            _ => Err(D::Error::custom(format!("Expected string for Layer, got {}", val))),
        }
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    let old_path =
        std::env::var("OLD_MEMORY_PATH").unwrap_or_else(|_| "/tmp/memory_l1".to_string());
    let new_path = std::env::var("MEMORY_L1_PATH")
        .unwrap_or_else(|_| "/tmp/memory_v2_l1".to_string());

    println!("Migration: {} → {}", old_path, new_path);

    // Open old sled DB
    let old_db = match sled::open(&old_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("ERROR: Cannot open old DB at {}: {}", old_path, e);
            std::process::exit(1);
        }
    };

    let tree = match old_db.open_tree(b"entries") {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ERROR: Cannot open 'entries' tree: {}", e);
            std::process::exit(1);
        }
    };

    // Verify new DB schema exists
    let new_conn = match Connection::open(&new_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ERROR: Cannot open new DB at {}: {}", new_path, e);
            eprintln!("Hint: Start v2 once first to create the SQLite schema");
            std::process::exit(1);
        }
    };

    let count: i64 = new_conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .unwrap_or(0);
    if count > 0 {
        println!(
            "WARNING: New DB already has some entries. Appending (skip duplicates via INSERT OR IGNORE)."
        );
    }

    // Migrate
    let mut total = 0;
    let mut skipped = 0;
    let mut errors = 0;

    for item in tree.iter() {
        match item {
            Ok((k, v)) => {
                let key = String::from_utf8_lossy(&k).to_string();
                let value = String::from_utf8_lossy(&v).to_string();

                match serde_json::from_str::<Entry>(&value) {
                    Ok(entry) => {
                        let now = chrono::Utc::now();
                        let seq = total + 1;

                        let imp: i32 = entry.importance.as_i32();
                        let layer_str: String = entry.layer.as_str().to_string();

                        match new_conn.execute(
                            r#"INSERT OR IGNORE INTO entries
                               (key, id, value, importance, source, layer,
                                created_at, last_accessed, expires_at, seq)
                               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
                            params![
                                entry.key,
                                entry.id,
                                &value,
                                imp,
                                entry.source,
                                layer_str,
                                entry.created_at.timestamp(),
                                now.timestamp(),
                                entry.expires_at.map(|dt| dt.timestamp()),
                                seq,
                            ],
                        ) {
                            Ok(_) => total += 1,
                            Err(e) => {
                                eprintln!("Insert error [{}]: {}", entry.key, e);
                                errors += 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Parse error [{}]: {}", key, e);
                        skipped += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("Read error: {}", e);
                errors += 1;
            }
        }
    }

    println!();
    println!("=== Migration Result ===");
    println!("  Migrated:  {}", total);
    println!("  Skipped:   {}", skipped);
    println!("  Errors:    {}", errors);
    println!();

    if errors == 0 {
        println!("Success! Start v2 with:");
        println!("  RUST_LOG=info ./target/debug/memory-system-v2 serve");
    } else {
        println!("Completed with {} errors - check above for details.", errors);
    }
}
