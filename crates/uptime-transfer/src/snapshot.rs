//! A streaming, sealed JSONL format. Rows are sensitive; reports contain hashes only.

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::schema::{Kind, TABLES};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Header {
        version: u32,
        schema: String,
        created_at: String,
        engine: String,
    },
    Table {
        name: String,
    },
    Row {
        values: Vec<Option<String>>,
    },
    TableEnd {
        summary: Summary,
    },
    End {
        sha256: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub table: String,
    pub rows: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub format: u32,
    pub schema: String,
    pub sha256: String,
    pub tables: Vec<Summary>,
    #[serde(default)]
    pub truncated_timestamps: u64,
}

pub fn schema_hash() -> String {
    let schema: Vec<_> = TABLES.iter().map(|t| (t.name, t.columns, t.keys)).collect();
    hex::encode(Sha256::digest(
        serde_json::to_vec(&schema).unwrap_or_default(),
    ))
}

pub fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .context("creating a new private artifact")
}

pub struct Writer {
    file: BufWriter<File>,
    hash: Sha256,
}

impl Writer {
    pub fn new(path: &Path, engine: &str) -> Result<Self> {
        let mut this = Self {
            file: BufWriter::new(private_file(path)?),
            hash: Sha256::new(),
        };
        this.push(&Record::Header {
            version: 1,
            schema: schema_hash(),
            created_at: jiff::Timestamp::now().to_string(),
            engine: engine.to_owned(),
        })?;
        Ok(this)
    }

    pub fn push(&mut self, record: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        self.hash.update(&bytes);
        self.hash.update(b"\n");
        self.file.write_all(&bytes)?;
        self.file.write_all(b"\n")?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let end = Record::End {
            sha256: hex::encode(self.hash.finalize()),
        };
        serde_json::to_writer(&mut self.file, &end)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(())
    }
}

pub struct Reader {
    file: BufReader<File>,
}

impl Reader {
    pub fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            file: BufReader::new(File::open(path)?),
        })
    }

    pub fn read_record(&mut self) -> Result<Option<(Record, Vec<u8>)>> {
        let mut bytes = Vec::new();
        let mut limited = std::io::Read::take(&mut self.file, 8 * 1024 * 1024 + 1);
        let size = limited.read_until(b'\n', &mut bytes)?;
        if size == 0 {
            return Ok(None);
        }
        ensure!(
            size <= 8 * 1024 * 1024 && bytes.ends_with(b"\n"),
            "snapshot line is oversized or incomplete"
        );
        let record = serde_json::from_slice(&bytes).context("invalid snapshot record")?;
        Ok(Some((record, bytes)))
    }
}

pub fn row_hash(hash: &mut Sha256, values: &[Option<String>]) -> Result<()> {
    let bytes = serde_json::to_vec(values)?;
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    Ok(())
}

pub fn normalize(kind: Kind, value: Option<String>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = match kind {
        Kind::Integer => value.parse::<i64>()?.to_string(),
        Kind::Boolean => match value.as_str() {
            "true" | "t" | "1" => "1".to_owned(),
            "false" | "f" | "0" => "0".to_owned(),
            _ => anyhow::bail!("invalid boolean"),
        },
        Kind::Timestamp => format!("{:.9}", value.parse::<jiff::Timestamp>()?),
        Kind::Date => value.parse::<jiff::civil::Date>()?.to_string(),
        Kind::Json => serde_json::to_string(&value.parse::<serde_json::Value>()?)?,
        Kind::Text => value,
    };
    Ok(Some(value))
}

pub fn validate(path: &Path) -> Result<Report> {
    let mut reader = Reader::new(path)?;
    let mut hash = Sha256::new();
    let mut row_digest = Sha256::new();
    let mut rows = 0;
    let mut tables = Vec::new();
    let mut active = false;
    let mut header = false;
    while let Some((record, bytes)) = reader.read_record()? {
        if let Record::End { sha256 } = record {
            ensure!(
                header && !active && tables.len() == TABLES.len(),
                "snapshot is missing tables"
            );
            ensure!(
                sha256 == hex::encode(hash.finalize()),
                "snapshot checksum mismatch"
            );
            ensure!(
                reader.read_record()?.is_none(),
                "data follows the snapshot seal"
            );
            return Ok(Report {
                format: 1,
                schema: schema_hash(),
                sha256,
                tables,
                truncated_timestamps: 0,
            });
        }
        hash.update(&bytes);
        match record {
            Record::Header {
                version, schema, ..
            } => {
                ensure!(
                    !header
                        && tables.is_empty()
                        && !active
                        && version == 1
                        && schema == schema_hash(),
                    "unsupported snapshot schema"
                );
                header = true;
            }
            Record::Table { name } => {
                ensure!(
                    header && !active && TABLES.get(tables.len()).is_some_and(|t| t.name == name),
                    "unexpected table order"
                );
                active = true;
                rows = 0;
                row_digest = Sha256::new();
            }
            Record::Row { values } => {
                ensure!(active, "row outside a table");
                let table = &TABLES[tables.len()];
                ensure!(values.len() == table.columns.len(), "wrong column count");
                for ((_, kind), value) in table.columns.iter().zip(&values) {
                    ensure!(
                        normalize(*kind, value.clone())? == *value,
                        "noncanonical snapshot value"
                    );
                }
                row_hash(&mut row_digest, &values)?;
                rows += 1;
            }
            Record::TableEnd { summary } => {
                ensure!(
                    active
                        && summary.table == TABLES[tables.len()].name
                        && summary.rows == rows
                        && summary.sha256 == hex::encode(row_digest.clone().finalize()),
                    "table checksum mismatch"
                );
                tables.push(summary);
                active = false;
            }
            Record::End { .. } => unreachable!(),
        }
    }
    anyhow::bail!("snapshot is incomplete (missing seal)")
}
