//! Interchange with slskd's relay share repository. Only masked paths cross into
//! the controller's index; the uploaded SQLite database is never retained.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, PrepFlags, config::DbConfig, limits::Limit, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use slskr_api::{options::SharesOptions, shares::Share};

use super::{Attribute, LOCAL, Snapshot as LocalSnapshot, count, valid_remote_component};

// The SQL sqlite_schema records for these tables match slskd's validator.
const TABLES: &[(&str, &str)] = &[
    ("version", "CREATE TABLE version (a INTEGER PRIMARY KEY)"),
    (
        "scans",
        "CREATE TABLE scans (timestamp INTEGER PRIMARY KEY, options TEXT NOT NULL, end INTEGER DEFAULT NULL, suspect INTEGER DEFAULT 0)",
    ),
    (
        "directories",
        "CREATE TABLE directories (name TEXT PRIMARY KEY, timestamp INTEGER NOT NULL)",
    ),
    (
        "filenames",
        "CREATE VIRTUAL TABLE filenames USING fts5(maskedFilename)",
    ),
    (
        "filenames_data",
        "CREATE TABLE 'filenames_data'(id INTEGER PRIMARY KEY, block BLOB)",
    ),
    (
        "filenames_idx",
        "CREATE TABLE 'filenames_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID",
    ),
    (
        "filenames_content",
        "CREATE TABLE 'filenames_content'(id INTEGER PRIMARY KEY, c0)",
    ),
    (
        "filenames_docsize",
        "CREATE TABLE 'filenames_docsize'(id INTEGER PRIMARY KEY, sz BLOB)",
    ),
    (
        "filenames_config",
        "CREATE TABLE 'filenames_config'(k PRIMARY KEY, v) WITHOUT ROWID",
    ),
    (
        "files",
        "CREATE TABLE files (maskedFilename TEXT PRIMARY KEY, originalFilename TEXT NOT NULL, size BIGINT NOT NULL, touchedAt TEXT NOT NULL, code INTEGER DEFAULT 1 NOT NULL, extension TEXT, attributeJson TEXT NOT NULL, timestamp INTEGER NOT NULL)",
    ),
];

const INDEXES: &[(&str, &str, Option<&str>)] = &[
    (
        "idx_files_timestamp",
        "files",
        Some("CREATE INDEX idx_files_timestamp ON files(timestamp)"),
    ),
    (
        "idx_directories_timestamp",
        "directories",
        Some("CREATE INDEX idx_directories_timestamp ON directories(timestamp)"),
    ),
    (
        "idx_scans_timestamp",
        "scans",
        Some("CREATE INDEX idx_scans_timestamp ON scans(timestamp DESC)"),
    ),
    ("sqlite_autoindex_files_1", "files", None),
    ("sqlite_autoindex_directories_1", "directories", None),
];

const MAX_SHARES: usize = 4_096;
const MAX_DIRECTORIES: usize = 1_000_000;
const MAX_FILES: usize = 2_000_000;
const MAX_PATH_BYTES: usize = 8_192;
const MAX_ATTRIBUTE_BYTES: usize = 8_192;
const MAX_INDEX_BYTES: usize = 512 * 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(super) type Snapshots = BTreeMap<String, Arc<Snapshot>>;

#[derive(Debug, Serialize)]
pub(super) struct Snapshot {
    pub(super) shares: Vec<Share>,
    pub(super) directories: BTreeMap<String, Vec<RemoteFile>>,
    file_count: u32,
}

impl Snapshot {
    pub(super) fn counts(&self) -> (u32, u32) {
        (count(self.directories.len()), self.file_count)
    }

    pub(super) fn find_file(&self, parent: &str, name: &str) -> Option<&RemoteFile> {
        let files = self.directories.get(parent)?;
        let index = files
            .binary_search_by(|file| file.name.as_str().cmp(name))
            .ok()?;
        Some(&files[index])
    }
}

#[derive(Debug, Serialize)]
pub(super) struct RemoteFile {
    pub(super) name: String,
    pub(super) size: u64,
    extension: String,
    code: u8,
    attributes: Vec<Attribute>,
}

impl RemoteFile {
    pub(super) fn wire(&self, filename: String) -> soulseek::File {
        soulseek::File {
            code: self.code,
            filename,
            size: self.size,
            extension: self.extension.clone(),
            attributes: self.attributes.iter().map(Attribute::wire).collect(),
        }
    }
}

pub(super) fn export(
    path: &Path,
    snapshot: Option<&LocalSnapshot>,
    options: &SharesOptions,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut db =
            Connection::open_with_flags(temporary.path(), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let tx = db.transaction()?;
        for &(name, sql) in TABLES {
            // FTS5 creates its shadow tables as part of creating filenames.
            if !name.starts_with("filenames_") {
                tx.execute(sql, [])?;
            }
        }
        for &(_, _, sql) in INDEXES {
            if let Some(sql) = sql {
                tx.execute(sql, [])?;
            }
        }
        if let Some(snapshot) = snapshot {
            let timestamp = snapshot.scanned_at.timestamp_millis();
            tx.execute(
                "INSERT INTO scans (timestamp, options, end) VALUES (?1, ?2, ?1)",
                params![timestamp, serde_json::to_string(options)?],
            )?;
            let mut add_dir =
                tx.prepare("INSERT INTO directories (name, timestamp) VALUES (?1, ?2)")?;
            let mut add_file = tx.prepare("INSERT INTO files (maskedFilename, originalFilename, size, touchedAt, code, extension, attributeJson, timestamp) VALUES (?1, ?2, ?3, '', 1, ?4, ?5, ?6)")?;
            let mut attributes = Vec::new();
            for share in snapshot.shares.values() {
                for (directory, files) in &share.directories {
                    add_dir.execute(params![directory, timestamp])?;
                    if files.is_empty() {
                        continue;
                    }
                    let mut original = share.root.clone();
                    original.extend(directory.split('\\').skip(1));
                    let mut masked = format!("{directory}\\");
                    let prefix = masked.len();
                    for file in files {
                        original.push(&file.name);
                        masked.push_str(&file.name);
                        // The native manifest does not record source mtimes; leave touchedAt empty.
                        attributes.clear();
                        serde_json::to_writer(&mut attributes, &file.attributes)?;
                        let json = std::str::from_utf8(&attributes)?;
                        add_file.execute(params![
                            masked,
                            original.to_string_lossy(),
                            i64::try_from(file.size)?,
                            file.extension,
                            json,
                            timestamp
                        ])?;
                        original.pop();
                        masked.truncate(prefix);
                    }
                }
            }
            tx.execute("INSERT INTO filenames SELECT maskedFilename FROM files", [])?;
        }
        tx.commit()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

fn validate_schema(db: &Connection) -> Result<()> {
    let mut schema = db.prepare_with_flags(
        "SELECT type, name, tbl_name, sql FROM main.sqlite_schema LIMIT ?1",
        PrepFlags::SQLITE_PREPARE_NO_VTAB,
    )?;
    let mut rows = schema.query([TABLES.len() + INDEXES.len() + 1])?;
    let mut found = HashSet::new();
    while let Some(row) = rows.next()? {
        let kind = row.get_ref(0)?.as_str()?;
        let name = row.get_ref(1)?.as_str()?;
        let table = row.get_ref(2)?.as_str()?;
        let sql = row.get_ref(3)?.as_str_or_null()?;
        // Exact table declarations validate the column definitions and prevent
        // replacing files/directories with a view, virtual table or generated columns.
        let expected = match kind {
            "table" if table == name => TABLES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|&(n, sql)| (n, Some(sql))),
            "index" => INDEXES
                .iter()
                .find(|(n, t, _)| *n == name && *t == table)
                .map(|&(n, _, sql)| (n, sql)),
            _ => None,
        };
        ensure!(
            expected.is_some_and(|(name, expected)| expected == sql && found.insert(name)),
            "unexpected SQLite schema entry in relay snapshot"
        );
    }
    ensure!(
        found.len() == TABLES.len() + INDEXES.len(),
        "incomplete relay SQLite schema"
    );
    Ok(())
}

fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path
            .split('\\')
            .all(|part| valid_remote_component(part) && !part.chars().any(char::is_control))
}

fn validate_metadata(shares: &[Share]) -> Result<()> {
    ensure!(shares.len() <= MAX_SHARES, "too many relay shares");
    for share in shares {
        ensure!(
            !share.id.is_empty()
                && share.id.len() <= 512
                && !share.id.chars().any(char::is_control),
            "invalid relay share ID"
        );
        if let Some(remote) = &share.remote_path {
            ensure!(
                valid_path(remote.trim_end_matches('\\')),
                "invalid relay share root"
            );
        } else {
            ensure!(share.is_excluded, "missing relay share root");
        }
    }
    Ok(())
}

pub(super) fn import(path: &Path, metadata: serde_json::Value) -> Result<Snapshot> {
    let shares: Vec<Share> =
        serde_json::from_value(metadata).context("invalid relay share metadata")?;
    validate_metadata(&shares)?;
    let file = fs::symlink_metadata(path)?;
    ensure!(
        file.is_file() && file.len() <= MAX_DATABASE_BYTES,
        "invalid relay SQLite file size or type"
    );
    let mut db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false)?;
    db.set_limit(Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024)?;
    db.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    // Keep the schema and both result sets on the same database snapshot.
    let tx = db.transaction()?;
    validate_schema(&tx)?;
    let mut directories = BTreeMap::<String, Vec<RemoteFile>>::new();
    let mut budget = MAX_INDEX_BYTES;
    {
        let mut stmt = tx.prepare_with_flags(
            "SELECT name FROM main.directories NOT INDEXED LIMIT ?1",
            PrepFlags::SQLITE_PREPARE_NO_VTAB,
        )?;
        let mut rows = stmt.query([MAX_DIRECTORIES + 1])?;
        while let Some(row) = rows.next()? {
            ensure!(
                directories.len() < MAX_DIRECTORIES,
                "too many relay directories"
            );
            let name = row.get_ref(0)?.as_str()?;
            ensure!(valid_path(name), "invalid masked relay directory");
            charge(
                &mut budget,
                name.len() + size_of::<(String, Vec<RemoteFile>)>(),
            )?;
            ensure!(
                directories.insert(name.to_owned(), Vec::new()).is_none(),
                "duplicate relay directory"
            );
        }
    }
    let mut file_count = 0;
    {
        // originalFilename, FTS5, scans and the unused metadata columns never
        // participate in the controller's reads or file resolution.
        let mut stmt = tx.prepare_with_flags("SELECT maskedFilename, code, size, extension, attributeJson FROM main.files NOT INDEXED LIMIT ?1", PrepFlags::SQLITE_PREPARE_NO_VTAB)?;
        let mut rows = stmt.query([MAX_FILES + 1])?;
        while let Some(row) = rows.next()? {
            ensure!(file_count < MAX_FILES, "too many relay files");
            let masked = row.get_ref(0)?.as_str()?;
            ensure!(valid_path(masked), "invalid masked relay filename");
            let (parent, name) = masked
                .rsplit_once('\\')
                .context("missing masked relay file parent")?;
            let files = directories
                .get_mut(parent)
                .context("relay file parent missing from directories")?;
            let code = row.get::<_, u8>(1)?;
            let size = u64::try_from(row.get::<_, i64>(2)?)?;
            let extension = row.get_ref(3)?.as_str_or_null()?.unwrap_or_default();
            ensure!(
                extension.len() <= 1_024 && !extension.chars().any(char::is_control),
                "invalid relay file extension"
            );
            let attribute_json = row.get_ref(4)?.as_str()?;
            ensure!(
                attribute_json.len() <= MAX_ATTRIBUTE_BYTES,
                "relay file attribute JSON too large"
            );
            let attributes: Vec<Attribute> =
                serde_json::from_str(attribute_json).context("invalid relay file attributes")?;
            ensure!(attributes.len() <= 64, "too many relay file attributes");
            charge(
                &mut budget,
                size_of::<RemoteFile>()
                    + name.len()
                    + extension.len()
                    + attributes.capacity() * size_of::<Attribute>(),
            )?;
            files.push(RemoteFile {
                name: name.to_owned(),
                size,
                extension: extension.to_owned(),
                code,
                attributes,
            });
            file_count += 1;
        }
    }
    for files in directories.values_mut() {
        files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        ensure!(
            !files.windows(2).any(|pair| pair[0].name == pair[1].name),
            "duplicate relay filename"
        );
    }
    tx.commit()?;
    Ok(Snapshot {
        shares,
        directories,
        file_count: count(file_count),
    })
}

fn charge(remaining: &mut usize, amount: usize) -> Result<()> {
    *remaining = remaining
        .checked_sub(amount)
        .context("relay share index exceeds its size limit")?;
    Ok(())
}

pub(super) fn validate_agent(agent: &str) -> Result<()> {
    ensure!(
        !agent.trim().is_empty()
            && agent.len() <= 256
            && !agent.chars().any(char::is_control)
            && !agent.eq_ignore_ascii_case(LOCAL),
        "invalid relay share agent"
    );
    Ok(())
}

#[derive(Serialize)]
struct CachedSnapshot<'a> {
    agent: &'a str,
    snapshot: &'a Snapshot,
}

fn cache_path(directory: &Path, agent: &str) -> PathBuf {
    directory.join(format!("{:x}.json", Sha256::digest(agent.as_bytes())))
}

pub(super) fn stage_cache(
    directory: &Path,
    agent: &str,
    snapshot: &Snapshot,
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    fs::create_dir_all(directory)?;
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    let mut writer = BufWriter::new(temp.as_file_mut());
    serde_json::to_writer(&mut writer, &CachedSnapshot { agent, snapshot })?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    Ok((temp, cache_path(directory, agent)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shares::{ScanOptions, ShareContents, ShareFile, local_shares, relay_contents};
    use serde_json::json;

    fn metadata() -> serde_json::Value {
        json!([
            {"id":"music-id","alias":"music","isExcluded":false,"localPath":"/remote/music",
             "raw":"[music]/remote/music","remotePath":"music","directories":2,"files":1}
        ])
    }

    fn relay_fixture(path: &Path) {
        export(path, None, &SharesOptions::default()).unwrap();
        let db = Connection::open(path).unwrap();
        db.execute_batch("
            INSERT INTO directories VALUES ('music', 0), ('music\\albums', 0),
                ('music\\empty', 0), ('music2', 0);
            INSERT INTO files VALUES ('music\\albums\\song.flac', '/never/open/this/path', 900, '', 1,
                'flac', '[{\"type\":\"Length\",\"value\":8},{\"type\":\"BitDepth\",\"value\":24},{\"type\":42,\"value\":7}]', 0);
        ").unwrap();
    }

    #[test]
    fn exports_slskd_schema_and_indexes_the_masked_path() {
        let temp = tempfile::tempdir().unwrap();
        let spec = format!("[music]{}", temp.path().display());
        let options = slskr_api::options::Options {
            shares: SharesOptions {
                directories: vec![spec.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let snapshot = LocalSnapshot {
            options: ScanOptions::new(&options),
            scanned_at: chrono::DateTime::from_timestamp_millis(1_000).unwrap(),
            shares: BTreeMap::from([(
                "music".into(),
                ShareContents {
                    root: temp.path().to_owned(),
                    configured_root: temp.path().to_owned(),
                    directories: BTreeMap::from([(
                        "music".into(),
                        vec![ShareFile {
                            name: "needle.flac".into(),
                            size: 7,
                            extension: "flac".into(),
                            attributes: vec![Attribute {
                                kind: super::super::AttributeType::Named(
                                    super::super::NamedAttribute::Length,
                                ),
                                value: 8,
                            }],
                        }],
                    )]),
                    file_count: 1,
                },
            )]),
        };
        let path = temp.path().join("wire.db");
        export(&path, Some(&snapshot), &options.shares).unwrap();
        let shares = serde_json::to_value(local_shares(&options, Some(&snapshot))).unwrap();
        assert_eq!(shares[0]["raw"], spec);
        assert_eq!(shares[0]["remotePath"], "music");
        let db = Connection::open(&path).unwrap();
        let result: (String, String) = db.query_row(
            "SELECT files.maskedFilename, files.originalFilename FROM filenames INNER JOIN files USING (maskedFilename) WHERE filenames MATCH ?1",
            ["needle"], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(
            result,
            (
                "music\\needle.flac".into(),
                temp.path()
                    .join("needle.flac")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        drop(db);
        let imported = import(&path, shares).unwrap();
        let file = imported
            .find_file("music", "needle.flac")
            .unwrap()
            .wire("needle.flac".into());
        assert_eq!(file.size, 7);
        assert_eq!(
            file.attributes,
            [soulseek::FileAttribute::new(
                soulseek::FileAttributeType::Duration,
                8
            )]
        );
    }

    #[test]
    fn import_preserves_attributes_and_uses_only_masked_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wire.db");
        relay_fixture(&path);
        let snapshot = import(&path, metadata()).unwrap();
        let file = snapshot
            .find_file("music\\albums", "song.flac")
            .unwrap()
            .wire("song.flac".into());
        assert_eq!(file.size, 900);
        assert_eq!(
            file.attributes,
            [
                soulseek::FileAttribute::new(soulseek::FileAttributeType::Duration, 8),
                soulseek::FileAttribute::new(soulseek::FileAttributeType::BitDepth, 24),
                soulseek::FileAttribute::new(soulseek::FileAttributeType::Unknown(42), 7),
            ]
        );
        let contents = relay_contents(&snapshot, &snapshot.shares[0]);
        let directories: Vec<_> = contents.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(directories, ["music\\albums", "music\\empty"]);

        let cache = temp.path().join("agents");
        let (staged, destination) = stage_cache(&cache, "../one", &snapshot).unwrap();
        assert_eq!(destination.parent(), Some(cache.as_path()));
        assert!(!destination.exists());
        staged.persist(&destination).unwrap();
        let persisted = fs::read_to_string(&destination).unwrap();
        assert!(!persisted.contains("/never/open/this/path"));
        let value: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        assert_eq!(value["agent"], "../one");
        assert_eq!(
            value["snapshot"]["directories"]["music\\albums"][0]["size"],
            900
        );
    }

    #[test]
    fn rejects_views_triggers_altered_columns_and_unsafe_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wire.db");
        for sql in [
            "CREATE TRIGGER rogue AFTER INSERT ON files BEGIN SELECT 1; END",
            "DROP TABLE files; CREATE VIEW files AS SELECT 'fake' AS maskedFilename",
            "DROP TABLE files; CREATE VIRTUAL TABLE files USING fts5(maskedFilename)",
            "ALTER TABLE directories ADD COLUMN invalid TEXT",
            "UPDATE files SET size = -1",
            "UPDATE files SET maskedFilename = 'music\\..\\secret'",
            "UPDATE files SET attributeJson = 'not json'",
        ] {
            relay_fixture(&path);
            let db = Connection::open(&path).unwrap();
            db.execute_batch(sql).unwrap();
            drop(db);
            assert!(import(&path, metadata()).is_err(), "{sql}");
        }
    }
}
