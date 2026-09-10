//! Disposable, derived SQLite persistence for Apple Notes runtime data.
//!
//! This crate never contacts Notes.app and is not a `NotesBackend`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use notes_core::{perf, Account, Folder, Note, NoteId, NoteSummary};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use thiserror::Error;

pub const SCHEMA_VERSION: i32 = 1;
static NEXT_QUARANTINE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("injected cache failure: {0}")]
    Injected(&'static str),
    #[error("could not open cache: {0}")]
    Open(#[source] rusqlite::Error),
    #[error("cache schema version {found} is unsupported")]
    UnsupportedVersion { found: i32 },
    #[error("cache database error: {0}")]
    Sqlite(#[source] rusqlite::Error),
    #[error("cache payload could not be decoded: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("cache filesystem error: {0}")]
    Io(#[source] std::io::Error),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CachedState {
    pub accounts: Vec<Account>,
    pub folders: Vec<Folder>,
    pub notes: Vec<NoteSummary>,
    pub last_successful_refresh: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheInfo {
    pub path: PathBuf,
    pub schema_version: i32,
    pub accounts: usize,
    pub folders: usize,
    pub notes: usize,
    pub full_notes: usize,
    pub last_successful_refresh: Option<String>,
}

pub struct SqliteNotesCache {
    path: PathBuf,
    connection: Connection,
    #[cfg(test)]
    replace_snapshot_failure: std::cell::Cell<bool>,
}

pub trait CacheStore {
    fn load_bootstrap(&self) -> Result<CachedState, CacheError>;
    fn replace_snapshot(&mut self, state: &CachedState) -> Result<(), CacheError>;
    fn load_note(&self, id: &NoteId) -> Result<Option<Note>, CacheError>;
    fn upsert_note(&mut self, note: &Note) -> Result<(), CacheError>;
    fn remove_note(&self, id: &NoteId) -> Result<(), CacheError>;
}

impl SqliteNotesCache {
    /// Opens a derived cache, quarantining only a corrupt or newer-schema cache.
    pub fn open_or_recover(path: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let path = path.into();
        match Self::open(path.clone()) {
            Ok(cache) => Ok(cache),
            Err(error) if Self::is_recoverable(&error) => {
                Self::quarantine(&path)?;
                Self::open(path)
            }
            Err(error) => Err(error),
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(CacheError::Io)?;
        }
        let connection = Connection::open(&path).map_err(CacheError::Open)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
            )
            .map_err(CacheError::Sqlite)?;
        let version: i32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(CacheError::Sqlite)?;
        if version > SCHEMA_VERSION {
            return Err(CacheError::UnsupportedVersion { found: version });
        }
        if version == 0 {
            connection.execute_batch("CREATE TABLE IF NOT EXISTS cache_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS snapshots (key TEXT PRIMARY KEY, payload TEXT NOT NULL); CREATE TABLE IF NOT EXISTS full_notes (id TEXT PRIMARY KEY, payload TEXT NOT NULL); PRAGMA user_version = 1;").map_err(CacheError::Sqlite)?;
        }
        Ok(Self {
            path,
            connection,
            #[cfg(test)]
            replace_snapshot_failure: std::cell::Cell::new(false),
        })
    }

    pub fn application_support_path() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
            .join("Library/Application Support/apple-notes-tui/cache.sqlite3")
    }

    fn is_recoverable(error: &CacheError) -> bool {
        matches!(error, CacheError::UnsupportedVersion { .. })
            || matches!(error, CacheError::Open(source) | CacheError::Sqlite(source)
                if matches!(source.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)))
    }

    fn owned_paths(path: &Path) -> [PathBuf; 3] {
        [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ]
    }

    fn quarantine(path: &Path) -> Result<(), CacheError> {
        let suffix = format!(
            ".corrupt-{}-{}",
            std::process::id(),
            NEXT_QUARANTINE.fetch_add(1, Ordering::Relaxed)
        );
        for owned in Self::owned_paths(path) {
            if owned.exists() {
                let target = PathBuf::from(format!("{}{}", owned.display(), suffix));
                fs::rename(owned, target).map_err(CacheError::Io)?;
            }
        }
        Ok(())
    }

    pub fn clear_path(path: impl AsRef<Path>) -> Result<(), CacheError> {
        for owned in Self::owned_paths(path.as_ref()) {
            match fs::remove_file(owned) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(CacheError::Io(error)),
            }
        }
        Ok(())
    }

    pub fn load_bootstrap(&self) -> Result<CachedState, CacheError> {
        self.connection
            .query_row(
                "SELECT payload FROM snapshots WHERE key = 'state'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(CacheError::Sqlite)?
            .map(|payload| serde_json::from_str(&payload).map_err(CacheError::Decode))
            .transpose()
            .map(|state| state.unwrap_or_default())
    }

    pub fn replace_snapshot(&mut self, state: &CachedState) -> Result<(), CacheError> {
        let payload = serde_json::to_string(state).map_err(CacheError::Decode)?;
        let transaction = self.connection.transaction().map_err(CacheError::Sqlite)?;
        transaction.execute("INSERT INTO snapshots(key, payload) VALUES ('state', ?1) ON CONFLICT(key) DO UPDATE SET payload = excluded.payload", params![payload]).map_err(CacheError::Sqlite)?;
        #[cfg(test)]
        if self.replace_snapshot_failure.replace(false) {
            return Err(CacheError::Injected("after snapshot write"));
        }
        transaction.execute("INSERT INTO cache_meta(key, value) VALUES ('last_successful_refresh', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![state.last_successful_refresh.clone().unwrap_or_default()]).map_err(CacheError::Sqlite)?;
        transaction.commit().map_err(CacheError::Sqlite)
    }

    pub fn load_note(&self, id: &NoteId) -> Result<Option<Note>, CacheError> {
        self.connection
            .query_row(
                "SELECT payload FROM full_notes WHERE id = ?1",
                params![id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(CacheError::Sqlite)?
            .map(|payload| serde_json::from_str(&payload).map_err(CacheError::Decode))
            .transpose()
    }

    pub fn upsert_note(&self, note: &Note) -> Result<(), CacheError> {
        let payload = serde_json::to_string(note).map_err(CacheError::Decode)?;
        self.connection.execute("INSERT INTO full_notes(id, payload) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET payload = excluded.payload", params![note.summary.id.as_str(), payload]).map_err(CacheError::Sqlite)?;
        Ok(())
    }

    /// Removes only one authoritative delegated-delete target. Snapshot absence
    /// is never used as global deletion authority.
    pub fn remove_note(&self, id: &NoteId) -> Result<(), CacheError> {
        self.connection
            .execute("DELETE FROM full_notes WHERE id = ?1", params![id.as_str()])
            .map_err(CacheError::Sqlite)?;
        Ok(())
    }

    pub fn clear(self) -> Result<(), CacheError> {
        let path = self.path.clone();
        drop(self);
        Self::clear_path(path)
    }

    pub fn info(&self) -> Result<CacheInfo, CacheError> {
        let state = self.load_bootstrap()?;
        let full_notes = self
            .connection
            .query_row("SELECT count(*) FROM full_notes", [], |row| {
                row.get::<_, usize>(0)
            })
            .map_err(CacheError::Sqlite)?;
        Ok(CacheInfo {
            path: self.path.clone(),
            schema_version: SCHEMA_VERSION,
            accounts: state.accounts.len(),
            folders: state.folders.len(),
            notes: state.notes.len(),
            full_notes,
            last_successful_refresh: state.last_successful_refresh,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    fn arm_replace_snapshot_failure_once(&self) {
        self.replace_snapshot_failure.set(true);
    }
}

impl CacheStore for SqliteNotesCache {
    fn load_bootstrap(&self) -> Result<CachedState, CacheError> {
        let started = Instant::now();
        let result = SqliteNotesCache::load_bootstrap(self);
        perf::event(
            "cache.load_bootstrap",
            None,
            started,
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }
    fn replace_snapshot(&mut self, state: &CachedState) -> Result<(), CacheError> {
        let started = Instant::now();
        let result = SqliteNotesCache::replace_snapshot(self, state);
        perf::event(
            "cache.replace_snapshot",
            None,
            started,
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }
    fn load_note(&self, id: &NoteId) -> Result<Option<Note>, CacheError> {
        let started = Instant::now();
        let result = SqliteNotesCache::load_note(self, id);
        perf::event(
            "cache.load_note",
            Some(id.as_str()),
            started,
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }
    fn upsert_note(&mut self, note: &Note) -> Result<(), CacheError> {
        let started = Instant::now();
        let result = SqliteNotesCache::upsert_note(self, note);
        perf::event(
            "cache.upsert_note",
            Some(note.summary.id.as_str()),
            started,
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }
    fn remove_note(&self, id: &NoteId) -> Result<(), CacheError> {
        let started = Instant::now();
        let result = SqliteNotesCache::remove_note(self, id);
        perf::event(
            "cache.remove_note",
            Some(id.as_str()),
            started,
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notes_core::{
        AccountId, AttachmentAccessStatus, AttachmentId, AttachmentKind, FolderId, FolderParent,
        NoteDate,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DB: AtomicUsize = AtomicUsize::new(0);

    fn path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "notes-cache-{}-{}-{}.sqlite3",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEST_DB.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn sample_state(label: &str) -> CachedState {
        let account = Account {
            id: AccountId::from("account"),
            name: "Grüße Köln".into(),
            is_default: true,
            is_upgraded: true,
            default_folder_id: Some(FolderId::from("family")),
        };
        let folder = Folder {
            id: FolderId::from("family"),
            account_id: account.id.clone(),
            name: "Семья 🚀".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        };
        let date = NoteDate::new("2026-08-28");
        CachedState {
            accounts: vec![account],
            folders: vec![folder],
            notes: vec![NoteSummary {
                id: NoteId::from(label),
                folder_id: FolderId::from("family"),
                name: "Русская заметка 🚀".into(),
                creation_date: date.clone(),
                modification_date: date,
                password_protected: false,
                shared: false,
                attachment_count: Some(0),
            }],
            last_successful_refresh: Some("2026-08-28T12:00:00Z".into()),
        }
    }
    fn sample_note(id: &str, body: &str) -> Note {
        let date = NoteDate::new("2026-08-28");
        let note_id = NoteId::from(id);
        Note {
            summary: NoteSummary {
                id: note_id.clone(),
                folder_id: FolderId::from("family"),
                name: "Русская заметка 🚀".into(),
                creation_date: date.clone(),
                modification_date: date.clone(),
                password_protected: false,
                shared: false,
                attachment_count: Some(1),
            },
            account_id: AccountId::from("account"),
            body_html: format!("<div>{body}</div>"),
            plaintext: body.into(),
            attachments: vec![notes_core::AttachmentSummary {
                id: AttachmentId::from("attachment"),
                note_id,
                display_name: "Grüße.pdf".into(),
                kind: AttachmentKind::Pdf,
                content_identifier: Some("cid:köln".into()),
                source_url: Some("https://example.test/🚀".into()),
                creation_date: date.clone(),
                modification_date: date,
                shared: false,
                preview_status: AttachmentAccessStatus::Available,
                export_status: AttachmentAccessStatus::Available,
            }],
        }
    }

    #[test]
    fn new_cache_creates_versioned_wal_schema_and_roundtrips_empty_state() {
        let cache = SqliteNotesCache::open(path()).unwrap();
        assert_eq!(cache.info().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(cache.load_bootstrap().unwrap(), CachedState::default());
        assert_eq!(
            cache
                .connection
                .query_row::<String, _, _>("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap(),
            "wal"
        );
    }

    #[test]
    fn unsupported_newer_schema_is_typed() {
        let path = path();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        assert!(matches!(
            SqliteNotesCache::open(path),
            Err(CacheError::UnsupportedVersion { found: 99 })
        ));
    }

    #[test]
    fn corrupt_cache_is_detected_and_recovered() {
        let path = path();
        fs::write(&path, b"this is not sqlite").unwrap();
        assert!(SqliteNotesCache::open(path.clone()).is_err());
        let cache = SqliteNotesCache::open_or_recover(path.clone()).unwrap();
        assert_eq!(cache.info().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(cache.load_bootstrap().unwrap(), CachedState::default());
        let quarantined = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .find_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(
                        "{}.corrupt-",
                        path.file_name().unwrap().to_string_lossy()
                    ))
                    .then_some(entry.path())
            })
            .unwrap();
        assert_eq!(fs::read(quarantined).unwrap(), b"this is not sqlite");
    }

    #[test]
    fn newer_schema_cache_is_quarantined_and_recreated() {
        let path = path();
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 2")
            .unwrap();
        let cache = SqliteNotesCache::open_or_recover(path.clone()).unwrap();
        assert_eq!(cache.info().unwrap().schema_version, SCHEMA_VERSION);
        assert!(fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&format!(
                    "{}.corrupt-",
                    path.file_name().unwrap().to_string_lossy()
                ))));
    }

    #[test]
    fn cache_clear_removes_only_configured_cache_and_owned_sidecars() {
        let path = path();
        for owned in SqliteNotesCache::owned_paths(&path) {
            fs::write(owned, b"owned").unwrap();
        }
        let sibling = PathBuf::from(format!("{}.backup-user", path.display()));
        let important = PathBuf::from(format!("{}.important.txt", path.display()));
        fs::write(&sibling, b"keep").unwrap();
        fs::write(&important, b"keep").unwrap();
        SqliteNotesCache::clear_path(&path).unwrap();
        assert!(SqliteNotesCache::owned_paths(&path)
            .iter()
            .all(|owned| !owned.exists()));
        assert_eq!(fs::read(sibling).unwrap(), b"keep");
        assert_eq!(fs::read(important).unwrap(), b"keep");
        SqliteNotesCache::clear_path(&path).unwrap();
        let cache = SqliteNotesCache::open_or_recover(path).unwrap();
        assert_eq!(cache.load_bootstrap().unwrap(), CachedState::default());
    }

    #[test]
    fn snapshot_roundtrip_preserves_accounts_folders_and_notes() {
        let path = path();
        let state = sample_state("note-a");
        let mut cache = SqliteNotesCache::open(&path).unwrap();
        cache.replace_snapshot(&state).unwrap();
        drop(cache);
        assert_eq!(
            SqliteNotesCache::open(path)
                .unwrap()
                .load_bootstrap()
                .unwrap(),
            state
        );
    }

    #[test]
    fn last_successful_refresh_roundtrips_and_cached_reads_do_not_update_it() {
        let path = path();
        let state = sample_state("note-a");
        let mut cache = SqliteNotesCache::open(&path).unwrap();
        cache.replace_snapshot(&state).unwrap();
        drop(cache);
        let cache = SqliteNotesCache::open(path).unwrap();
        assert_eq!(
            cache.info().unwrap().last_successful_refresh,
            state.last_successful_refresh
        );
        let _ = cache.load_bootstrap().unwrap();
        let _ = cache.load_note(&NoteId::from("missing")).unwrap();
        assert_eq!(
            cache.info().unwrap().last_successful_refresh,
            state.last_successful_refresh
        );
    }

    #[test]
    fn partial_snapshot_replacement_preserves_unrelated_full_notes_and_is_atomic() {
        let path = path();
        let old = sample_state("old");
        let new = sample_state("new");
        let mut cache = SqliteNotesCache::open(&path).unwrap();
        cache.replace_snapshot(&old).unwrap();
        cache.arm_replace_snapshot_failure_once();
        assert!(cache.replace_snapshot(&new).is_err());
        drop(cache);
        let reopened = SqliteNotesCache::open(path).unwrap();
        assert_eq!(reopened.load_bootstrap().unwrap(), old);
        assert_eq!(
            reopened.info().unwrap().last_successful_refresh,
            old.last_successful_refresh
        );
    }

    #[test]
    fn replace_snapshot_failure_injection_is_instance_local() {
        let mut a = SqliteNotesCache::open(path()).unwrap();
        let mut b = SqliteNotesCache::open(path()).unwrap();
        let state = sample_state("instance");
        a.arm_replace_snapshot_failure_once();
        assert!(a.replace_snapshot(&state).is_err());
        assert!(b.replace_snapshot(&state).is_ok());
        assert!(a.replace_snapshot(&state).is_ok());
    }

    #[test]
    fn full_note_roundtrip_preserves_body_metadata_and_attachments() {
        let path = path();
        let note = sample_note("full", "Русский текст\nGrüße aus Köln\nemoji 🚀");
        let cache = SqliteNotesCache::open(&path).unwrap();
        cache.upsert_note(&note).unwrap();
        drop(cache);
        assert_eq!(
            SqliteNotesCache::open(path)
                .unwrap()
                .load_note(&note.summary.id)
                .unwrap(),
            Some(note)
        );
    }

    #[test]
    fn full_notes_are_loaded_by_exact_note_id() {
        let path = path();
        let a = sample_note("a", "distinctive-A");
        let b = sample_note("b", "distinctive-B");
        let cache = SqliteNotesCache::open(&path).unwrap();
        cache.upsert_note(&a).unwrap();
        cache.upsert_note(&b).unwrap();
        drop(cache);
        let cache = SqliteNotesCache::open(path).unwrap();
        assert_eq!(cache.load_note(&a.summary.id).unwrap(), Some(a));
        assert_eq!(cache.load_note(&b.summary.id).unwrap(), Some(b));
        assert_eq!(cache.load_note(&NoteId::from("c")).unwrap(), None);
    }

    #[test]
    fn remove_note_removes_exact_full_note_only() {
        let path = path();
        let a = sample_note("a", "distinctive-A");
        let b = sample_note("b", "distinctive-B");
        let state = sample_state("snapshot-note");
        let mut cache = SqliteNotesCache::open(&path).unwrap();
        cache.replace_snapshot(&state).unwrap();
        cache.upsert_note(&a).unwrap();
        cache.upsert_note(&b).unwrap();
        cache.remove_note(&a.summary.id).unwrap();
        assert_eq!(cache.load_note(&a.summary.id).unwrap(), None);
        assert_eq!(cache.load_note(&b.summary.id).unwrap(), Some(b));
        assert_eq!(cache.load_bootstrap().unwrap(), state);
    }

    #[test]
    fn remove_note_is_idempotent() {
        let path = path();
        let note = sample_note("a", "distinctive-A");
        let cache = SqliteNotesCache::open(path).unwrap();
        cache.upsert_note(&note).unwrap();
        cache.remove_note(&note.summary.id).unwrap();
        cache.remove_note(&note.summary.id).unwrap();
        assert_eq!(cache.load_note(&note.summary.id).unwrap(), None);
    }

    #[test]
    fn cache_recovery_preserves_unrelated_sibling_files() {
        let path = path();
        fs::write(&path, b"broken").unwrap();
        let siblings = [
            (
                PathBuf::from(format!("{}.backup-user", path.display())),
                b"backup".as_slice(),
            ),
            (
                path.parent().unwrap().join(format!(
                    "another-{}.sqlite3",
                    path.file_name().unwrap().to_string_lossy()
                )),
                b"another".as_slice(),
            ),
            (
                path.parent().unwrap().join(format!(
                    "cache-other-{}",
                    path.file_name().unwrap().to_string_lossy()
                )),
                b"other".as_slice(),
            ),
        ];
        for (file, bytes) in &siblings {
            fs::write(file, bytes).unwrap();
        }
        SqliteNotesCache::open_or_recover(&path).unwrap();
        for (file, bytes) in siblings {
            assert_eq!(fs::read(file).unwrap(), bytes);
        }
    }

    #[test]
    fn repeated_quarantine_does_not_overwrite_previous_artifact() {
        let path = path();
        fs::write(&path, b"broken-one").unwrap();
        SqliteNotesCache::open_or_recover(&path).unwrap();
        fs::write(&path, b"broken-two").unwrap();
        SqliteNotesCache::open_or_recover(&path).unwrap();
        let artifacts: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(
                        "{}.corrupt-",
                        path.file_name().unwrap().to_string_lossy()
                    ))
                    .then_some(entry.path())
            })
            .collect();
        assert_eq!(artifacts.len(), 2);
        let bodies: Vec<_> = artifacts
            .into_iter()
            .map(fs::read)
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(bodies.contains(&b"broken-one".to_vec()));
        assert!(bodies.contains(&b"broken-two".to_vec()));
    }

    #[test]
    fn cache_clear_is_idempotent() {
        let path = path();
        for owned in SqliteNotesCache::owned_paths(&path) {
            fs::write(owned, b"x").unwrap();
        }
        let sibling = PathBuf::from(format!("{}.important.txt", path.display()));
        fs::write(&sibling, b"keep").unwrap();
        SqliteNotesCache::clear_path(&path).unwrap();
        SqliteNotesCache::clear_path(&path).unwrap();
        assert!(SqliteNotesCache::owned_paths(&path)
            .iter()
            .all(|p| !p.exists()));
        assert_eq!(fs::read(sibling).unwrap(), b"keep");
    }

    #[test]
    fn cache_clear_then_open_creates_valid_empty_cache() {
        let path = path();
        let state = sample_state("old");
        let note = sample_note("old", "old");
        let mut cache = SqliteNotesCache::open(&path).unwrap();
        cache.replace_snapshot(&state).unwrap();
        cache.upsert_note(&note).unwrap();
        drop(cache);
        SqliteNotesCache::clear_path(&path).unwrap();
        let cache = SqliteNotesCache::open_or_recover(path).unwrap();
        let info = cache.info().unwrap();
        assert_eq!(
            (
                info.accounts,
                info.folders,
                info.notes,
                info.full_notes,
                info.last_successful_refresh
            ),
            (0, 0, 0, 0, None)
        );
        assert_eq!(cache.load_bootstrap().unwrap(), CachedState::default());
        assert_eq!(cache.load_note(&note.summary.id).unwrap(), None);
    }

    #[test]
    fn cache_info_reports_schema_counts_and_refresh_metadata() {
        let path = path();
        let mut state = sample_state("a");
        let mut second = sample_state("b");
        state.accounts.push(Account {
            id: AccountId::from("second"),
            name: "Two".into(),
            is_default: false,
            is_upgraded: true,
            default_folder_id: None,
        });
        state.notes.push(second.notes.remove(0));
        let mut cache = SqliteNotesCache::open(&path).unwrap();
        cache.replace_snapshot(&state).unwrap();
        cache.upsert_note(&sample_note("a", "a")).unwrap();
        let info = cache.info().unwrap();
        assert_eq!(
            (
                info.schema_version,
                info.accounts,
                info.folders,
                info.notes,
                info.full_notes,
                info.last_successful_refresh
            ),
            (1, 2, 1, 2, 1, state.last_successful_refresh)
        );
    }

    #[test]
    fn cache_info_reports_valid_empty_cache_and_pragmas() {
        let cache = SqliteNotesCache::open(path()).unwrap();
        let info = cache.info().unwrap();
        assert_eq!(
            (
                info.schema_version,
                info.accounts,
                info.folders,
                info.notes,
                info.full_notes,
                info.last_successful_refresh
            ),
            (1, 0, 0, 0, 0, None)
        );
        assert_eq!(
            cache
                .connection
                .query_row::<String, _, _>("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            cache
                .connection
                .query_row::<i64, _, _>("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap(),
            1
        );
    }
}
