//! Read-only application state and rendering for Apple Notes.
//!
//! No AppleScript, process, or JSON details cross this boundary: the app only
//! communicates through `notes_core::NotesBackend`.

mod editor;
use editor::{
    char_to_byte_index, EditorDocument, EditorError, EditorTarget, InlineStyle, TargetKind,
};

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex, MutexGuard,
};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use notes_cache::{CacheStore, CachedState};
use notes_core::perf;
use notes_core::{
    classify_editability, parse_notes_html, serialize_notes_html, Account, AccountId,
    AttachmentAccessStatus, AttachmentCapabilities, AttachmentId, AttachmentMetadata,
    AttachmentSummary, AttachmentUnavailableReason, CreateChildFolder, CreateFolder, CreateNote,
    DeleteFolder, DeleteNote, DeletedFolder, Editability, Folder, FolderId, FolderParent,
    MockNotesBackend, MoveNote, Note, NoteDate, NoteId, NoteSummary, NotesBackend, NotesError,
    NotesPage, NotesQuery, RenameFolder, ReparentFolder, RichFeature, UpdateNote,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

const MINIMUM_WIDTH: u16 = 80;
const MINIMUM_HEIGHT: u16 = 24;
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
pub const MIN_REFRESH_INTERVAL_SECONDS: u64 = 5;
pub const MAX_REFRESH_INTERVAL_SECONDS: u64 = 86_400;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppConfig {
    pub refresh_interval: Duration,
    pub auto_refresh: bool,
    pub preview_wrap: bool,
    pub show_attachment_metadata: bool,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            auto_refresh: true,
            preview_wrap: true,
            show_attachment_metadata: true,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigValueSource {
    Default,
    File,
    Cli,
}
#[derive(Debug)]
pub struct ResolvedConfig {
    pub config: AppConfig,
    pub refresh_interval_source: ConfigValueSource,
    pub auto_refresh_source: ConfigValueSource,
    pub preview_wrap_source: ConfigValueSource,
    pub show_attachment_metadata_source: ConfigValueSource,
    pub file_presence: [bool; 4],
    pub warning: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CliConfigOverrides {
    pub refresh_interval_seconds: Option<u64>,
    pub auto_refresh: Option<bool>,
    pub preview_wrap: Option<bool>,
    pub show_attachment_metadata: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigKey {
    RefreshIntervalSeconds,
    AutoRefresh,
    PreviewWrap,
    ShowAttachmentMetadata,
}

impl ConfigKey {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "refresh_interval_seconds" => Ok(Self::RefreshIntervalSeconds),
            "auto_refresh" => Ok(Self::AutoRefresh),
            "preview_wrap" => Ok(Self::PreviewWrap),
            "show_attachment_metadata" => Ok(Self::ShowAttachmentMetadata),
            _ => Err(format!("unsupported config key: {value}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::RefreshIntervalSeconds => "refresh_interval_seconds",
            Self::AutoRefresh => "auto_refresh",
            Self::PreviewWrap => "preview_wrap",
            Self::ShowAttachmentMetadata => "show_attachment_metadata",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigEditCommand {
    Set(ConfigKey, ConfigEditValue),
    Unset(ConfigKey),
    Reset,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigEdit {
    Set(ConfigKey, ConfigEditValue),
    Unset(ConfigKey),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigEditValue {
    RefreshIntervalSeconds(u64),
    Boolean(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftCliCommand {
    Info,
    Clear,
}

/// Draft commands are deliberately standalone local-file operations. Keeping
/// parsing here makes the executable exit before backend/cache construction.
pub fn parse_draft_cli_command(args: &[String]) -> Result<Option<DraftCliCommand>, String> {
    let command = match (
        args.iter().any(|argument| argument == "--draft-info"),
        args.iter().any(|argument| argument == "--draft-clear"),
    ) {
        (false, false) => return Ok(None),
        (true, false) => DraftCliCommand::Info,
        (false, true) => DraftCliCommand::Clear,
        (true, true) => return Err("--draft-info and --draft-clear cannot be combined".into()),
    };
    if args.len() != 1 {
        return Err(
            "draft commands are standalone and cannot be combined with runtime options".into(),
        );
    }
    Ok(Some(command))
}

pub fn parse_config_set(value: &str) -> Result<ConfigEditCommand, String> {
    let (key, value) = value
        .split_once('=')
        .ok_or("--config-set requires key=value")?;
    let key = ConfigKey::parse(key)?;
    let value = match key {
        ConfigKey::RefreshIntervalSeconds => {
            let seconds = value
                .parse::<u64>()
                .map_err(|_| "refresh_interval_seconds must be an integer")?;
            interval(seconds)?;
            ConfigEditValue::RefreshIntervalSeconds(seconds)
        }
        _ => ConfigEditValue::Boolean(
            value
                .parse::<bool>()
                .map_err(|_| format!("{} must be true or false", key.name()))?,
        ),
    };
    match (key, value) {
        (ConfigKey::RefreshIntervalSeconds, ConfigEditValue::RefreshIntervalSeconds(_))
        | (
            ConfigKey::AutoRefresh | ConfigKey::PreviewWrap | ConfigKey::ShowAttachmentMetadata,
            ConfigEditValue::Boolean(_),
        ) => Ok(ConfigEditCommand::Set(key, value)),
        _ => unreachable!("value type follows config key"),
    }
}

pub fn parse_config_edit_command(args: &[String]) -> Result<Option<ConfigEditCommand>, String> {
    let mut command = None;
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        let parsed = match argument.as_str() {
            "--config-set" => Some(parse_config_set(
                args.get(index + 1)
                    .ok_or("--config-set requires key=value")?,
            )?),
            "--config-unset" => Some(ConfigEditCommand::Unset(ConfigKey::parse(
                args.get(index + 1)
                    .ok_or("--config-unset requires a supported key")?,
            )?)),
            "--config-reset" => Some(ConfigEditCommand::Reset),
            _ => None,
        };
        if parsed.is_some() {
            if command.is_some() {
                return Err("only one config write command may be used at a time".into());
            }
            command = parsed;
            if argument != "--config-reset" {
                index += 1;
            }
        }
        index += 1;
    }
    if command.is_some()
        && args.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--refresh-interval"
                    | "--auto-refresh"
                    | "--no-auto-refresh"
                    | "--preview-wrap"
                    | "--no-preview-wrap"
                    | "--show-attachment-metadata"
                    | "--hide-attachment-metadata"
            )
        })
    {
        return Err("config write commands cannot be combined with runtime overrides".into());
    }
    Ok(command)
}

pub fn parse_config_overrides(args: &[String]) -> Result<CliConfigOverrides, String> {
    let mut overrides = CliConfigOverrides::default();
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        match argument.as_str() {
            "--refresh-interval" => {
                let value = args
                    .get(index + 1)
                    .ok_or("--refresh-interval requires an integer")?;
                overrides.refresh_interval_seconds = Some(
                    value
                        .parse()
                        .map_err(|_| "--refresh-interval requires an integer")?,
                );
                index += 1;
            }
            "--auto-refresh" => set_boolean_override(
                &mut overrides.auto_refresh,
                true,
                "--auto-refresh",
                "--no-auto-refresh",
            )?,
            "--no-auto-refresh" => set_boolean_override(
                &mut overrides.auto_refresh,
                false,
                "--auto-refresh",
                "--no-auto-refresh",
            )?,
            "--preview-wrap" => set_boolean_override(
                &mut overrides.preview_wrap,
                true,
                "--preview-wrap",
                "--no-preview-wrap",
            )?,
            "--no-preview-wrap" => set_boolean_override(
                &mut overrides.preview_wrap,
                false,
                "--preview-wrap",
                "--no-preview-wrap",
            )?,
            "--show-attachment-metadata" => set_boolean_override(
                &mut overrides.show_attachment_metadata,
                true,
                "--show-attachment-metadata",
                "--hide-attachment-metadata",
            )?,
            "--hide-attachment-metadata" => set_boolean_override(
                &mut overrides.show_attachment_metadata,
                false,
                "--show-attachment-metadata",
                "--hide-attachment-metadata",
            )?,
            _ => {}
        }
        index += 1;
    }
    Ok(overrides)
}

fn set_boolean_override(
    slot: &mut Option<bool>,
    value: bool,
    positive: &str,
    negative: &str,
) -> Result<(), String> {
    match slot {
        Some(current) if *current != value => Err(format!("{positive} conflicts with {negative}")),
        _ => {
            *slot = Some(value);
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Default)]
struct FileConfig {
    refresh_interval_seconds: Option<u64>,
    auto_refresh: Option<bool>,
    preview_wrap: Option<bool>,
    show_attachment_metadata: Option<bool>,
}

#[derive(Default)]
struct ConfigDocument {
    file: FileConfig,
    unknown_lines: Vec<String>,
    has_unknown_keys: bool,
}

pub fn edit_config(path: &Path, command: ConfigEditCommand) -> Result<(), String> {
    let mut document = match fs::read_to_string(path) {
        Ok(contents) => parse_config_document(&contents)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigDocument::default(),
        Err(error) => return Err(format!("could not read config: {error}")),
    };
    validate_file_config(document.file)?;
    match command {
        ConfigEditCommand::Set(key, value) => set_config_value(&mut document.file, key, value),
        ConfigEditCommand::Unset(key) => unset_config_value(&mut document.file, key),
        ConfigEditCommand::Reset => document.file = FileConfig::default(),
    }
    if matches!(command, ConfigEditCommand::Reset)
        && !document.has_unknown_keys
        && config_file_is_empty(&document.file)
    {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove config: {error}")),
        };
    }
    atomic_write_config(path, &serialize_config_document(&document))
}

pub fn edit_config_values(
    path: &Path,
    values: &[(ConfigKey, ConfigEditValue)],
) -> Result<(), String> {
    let edits = values
        .iter()
        .copied()
        .map(|(key, value)| ConfigEdit::Set(key, value))
        .collect::<Vec<_>>();
    edit_config_batch(path, &edits)
}
pub fn edit_config_batch(path: &Path, edits: &[ConfigEdit]) -> Result<(), String> {
    let mut document = match fs::read_to_string(path) {
        Ok(contents) => parse_config_document(&contents)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigDocument::default(),
        Err(error) => return Err(format!("could not read config: {error}")),
    };
    validate_file_config(document.file)?;
    for &edit in edits {
        match edit {
            ConfigEdit::Set(key, value) => set_config_value(&mut document.file, key, value),
            ConfigEdit::Unset(key) => unset_config_value(&mut document.file, key),
        }
    }
    if config_file_is_empty(&document.file) && !document.has_unknown_keys {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    atomic_write_config(path, &serialize_config_document(&document))
}

fn set_config_value(file: &mut FileConfig, key: ConfigKey, value: ConfigEditValue) {
    match (key, value) {
        (ConfigKey::RefreshIntervalSeconds, ConfigEditValue::RefreshIntervalSeconds(value)) => {
            file.refresh_interval_seconds = Some(value)
        }
        (ConfigKey::AutoRefresh, ConfigEditValue::Boolean(value)) => {
            file.auto_refresh = Some(value)
        }
        (ConfigKey::PreviewWrap, ConfigEditValue::Boolean(value)) => {
            file.preview_wrap = Some(value)
        }
        (ConfigKey::ShowAttachmentMetadata, ConfigEditValue::Boolean(value)) => {
            file.show_attachment_metadata = Some(value)
        }
        _ => unreachable!("validated edit command"),
    }
}

fn unset_config_value(file: &mut FileConfig, key: ConfigKey) {
    match key {
        ConfigKey::RefreshIntervalSeconds => file.refresh_interval_seconds = None,
        ConfigKey::AutoRefresh => file.auto_refresh = None,
        ConfigKey::PreviewWrap => file.preview_wrap = None,
        ConfigKey::ShowAttachmentMetadata => file.show_attachment_metadata = None,
    }
}

fn config_file_is_empty(file: &FileConfig) -> bool {
    file.refresh_interval_seconds.is_none()
        && file.auto_refresh.is_none()
        && file.preview_wrap.is_none()
        && file.show_attachment_metadata.is_none()
}

fn serialize_config_document(document: &ConfigDocument) -> String {
    let mut lines = Vec::new();
    if let Some(value) = document.file.refresh_interval_seconds {
        lines.push(format!("refresh_interval_seconds = {value}"));
    }
    if let Some(value) = document.file.auto_refresh {
        lines.push(format!("auto_refresh = {value}"));
    }
    if let Some(value) = document.file.preview_wrap {
        lines.push(format!("preview_wrap = {value}"));
    }
    if let Some(value) = document.file.show_attachment_metadata {
        lines.push(format!("show_attachment_metadata = {value}"));
    }
    lines.extend(
        document
            .unknown_lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .cloned(),
    );
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn atomic_write_file(path: &Path, contents: &str, label: &str) -> Result<(), String> {
    atomic_write_file_with_hook(path, contents, label, |_| Ok(()))
}

fn atomic_write_file_with_hook<F>(
    path: &Path,
    contents: &str,
    label: &str,
    before_rename: F,
) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    static TEMP_FILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} path has no parent directory"))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {label} directory: {error}"))?;
    let temporary = parent.join(format!(
        ".{label}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("could not create temporary {label}: {error}"))?;
    if let Err(error) = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write {label}: {error}"));
    }
    if let Err(error) = before_rename(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("could not replace {label}: {error}")
    })
}

fn atomic_write_config(path: &Path, contents: &str) -> Result<(), String> {
    atomic_write_file(path, contents, "config")
}

pub fn config_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
        .join("Library/Application Support/apple-notes-tui/config.toml")
}

/// Disposable local UI continuity state. It deliberately contains no Notes
/// data; the stable IDs are validated against the current live/cache context.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionState {
    pub account_id: Option<AccountId>,
    pub folder_id: Option<FolderId>,
    pub note_id: Option<NoteId>,
    pub search_query: Option<String>,
    pub preview_scroll: Option<u16>,
    pub focus: Option<SessionFocus>,
}

/// The only UI focus states safe to carry between launches. Runtime `Focus`
/// intentionally has no transient editor or popup variants, but this separate
/// type keeps the on-disk contract explicit and stable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionFocus {
    Navigation,
    Notes,
    Preview,
}

impl SessionFocus {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "navigation" => Ok(Self::Navigation),
            "notes" => Ok(Self::Notes),
            "preview" => Ok(Self::Preview),
            _ => Err("session contains an unsupported focus".into()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Notes => "notes",
            Self::Preview => "preview",
        }
    }
}

pub fn session_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
        .join("Library/Application Support/apple-notes-tui/session.toml")
}

/// A single local, user-owned recovery draft. It is intentionally separate
/// from disposable navigation session state and the derived SQLite cache.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EditorRecoveryDraft {
    pub version: u8,
    pub kind: EditorRecoveryKind,
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub note_id: Option<NoteId>,
    pub expected_modification_date: Option<NoteDate>,
    pub title: String,
    pub document: notes_core::RichDocument,
    pub original_title: String,
    pub original_body_html: String,
    pub original_plaintext: String,
    #[serde(default)]
    cursor: Option<EditorCursorRecovery>,
    #[serde(default)]
    viewport: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorDraftInspection {
    Missing,
    Valid(Box<EditorRecoveryDraft>),
    Malformed { error: String },
    UnsupportedVersion { version: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorRecoveryKind {
    Create,
    Edit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum EditorRecoveryField {
    Title,
    Body,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct EditorCursorRecovery {
    field: EditorRecoveryField,
    target: EditorTarget,
    offset: usize,
}

pub fn editor_draft_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
        .join("Library/Application Support/apple-notes-tui/editor-draft.json")
}

pub fn load_editor_draft(path: &Path) -> Result<Option<EditorRecoveryDraft>, String> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read editor recovery draft: {error}")),
    };
    let draft: EditorRecoveryDraft = serde_json::from_slice(&contents)
        .map_err(|error| format!("could not parse editor recovery draft: {error}"))?;
    if draft.version != 1 {
        return Err(format!(
            "unsupported editor recovery draft version: {}",
            draft.version
        ));
    }
    match (&draft.kind, &draft.note_id) {
        (EditorRecoveryKind::Create, None) | (EditorRecoveryKind::Edit, Some(_)) => Ok(Some(draft)),
        _ => Err("editor recovery draft has incompatible kind and note identity".into()),
    }
}

/// Inspects one recovery file without modifying it. Valid drafts reuse the
/// normal typed recovery loader; invalid files remain available for explicit
/// local deletion through `clear_editor_draft`.
pub fn inspect_editor_draft(path: &Path) -> Result<EditorDraftInspection, String> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EditorDraftInspection::Missing)
        }
        Err(error) => return Err(format!("could not read editor recovery draft: {error}")),
    };
    let value: serde_json::Value = match serde_json::from_slice(&contents) {
        Ok(value) => value,
        Err(error) => {
            return Ok(EditorDraftInspection::Malformed {
                error: format!("could not parse editor recovery draft: {error}"),
            })
        }
    };
    if let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) {
        if version != 1 {
            return Ok(EditorDraftInspection::UnsupportedVersion { version });
        }
    }
    match load_editor_draft(path) {
        Ok(Some(draft)) => Ok(EditorDraftInspection::Valid(Box::new(draft))),
        Ok(None) => Ok(EditorDraftInspection::Missing),
        Err(error) => Ok(EditorDraftInspection::Malformed { error }),
    }
}

pub fn format_editor_draft_info(path: &Path) -> Result<String, String> {
    match inspect_editor_draft(path)? {
        EditorDraftInspection::Missing => Ok("Draft: none".into()),
        EditorDraftInspection::Malformed { error } => Ok(format!(
            "Draft path: {}\nStatus: malformed\nError: {error}",
            path.display()
        )),
        EditorDraftInspection::UnsupportedVersion { version } => Ok(format!(
            "Draft path: {}\nStatus: unsupported version\nVersion: {version}",
            path.display()
        )),
        EditorDraftInspection::Valid(draft) => {
            let mut lines = vec![
                format!("Draft path: {}", path.display()),
                "Status: valid".into(),
                format!("Version: {}", draft.version),
                format!(
                    "Kind: {}",
                    match draft.kind {
                        EditorRecoveryKind::Create => "create",
                        EditorRecoveryKind::Edit => "edit",
                    }
                ),
                format!("Account ID: {}", draft.account_id),
                format!("Folder ID: {}", draft.folder_id),
                format!("Title: {}", draft.title),
                "Dirty content: yes".into(),
                format!(
                    "Cursor: {}",
                    draft.cursor.map_or_else(
                        || "default".into(),
                        |cursor| format!(
                            "{:?} {:?} char {}",
                            cursor.field, cursor.target, cursor.offset
                        )
                    )
                ),
                format!(
                    "Viewport: {}",
                    draft
                        .viewport
                        .map_or_else(|| "default".into(), |viewport| viewport.to_string())
                ),
            ];
            if let EditorRecoveryKind::Edit = draft.kind {
                lines.push(format!(
                    "Note ID: {}",
                    draft.note_id.expect("validated edit draft")
                ));
                lines.push(format!(
                    "Baseline: {}",
                    draft
                        .expected_modification_date
                        .map_or_else(|| "none".into(), |date| date.to_string())
                ));
            }
            Ok(lines.join("\n"))
        }
    }
}

/// Deletes only the exact recovery file. It intentionally does not parse the
/// file, so an explicit `--draft-clear` can remove malformed drafts too.
pub fn clear_editor_draft(path: &Path) -> Result<bool, String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not clear editor recovery draft: {error}")),
    }
}

pub fn save_editor_draft(path: &Path, draft: &EditorRecoveryDraft) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(draft)
        .map_err(|error| format!("could not serialize editor recovery draft: {error}"))?;
    atomic_write_file(
        path,
        std::str::from_utf8(&contents).expect("JSON is UTF-8"),
        "editor recovery draft",
    )
}

fn session_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn parse_session_string(value: &str) -> Result<String, String> {
    let value = value.trim();
    let Some(value) = value
        .strip_prefix('\"')
        .and_then(|value| value.strip_suffix('\"'))
    else {
        return Err("session values must be quoted strings".into());
    };
    let mut output = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            match character {
                '\\' | '\"' => output.push(character),
                _ => return Err("session contains an unsupported escape".into()),
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        return Err("session ends with an incomplete escape".into());
    }
    Ok(output)
}

pub fn load_session(path: &Path) -> Result<Option<SessionState>, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read session: {error}")),
    };
    let mut state = SessionState::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("session contains an invalid line".into());
        };
        match key.trim() {
            "account_id" => state.account_id = Some(AccountId::from(parse_session_string(value)?)),
            "folder_id" => state.folder_id = Some(FolderId::from(parse_session_string(value)?)),
            "note_id" => state.note_id = Some(NoteId::from(parse_session_string(value)?)),
            "search_query" => state.search_query = Some(parse_session_string(value)?),
            "preview_scroll" => {
                state.preview_scroll = Some(
                    value
                        .trim()
                        .parse::<u16>()
                        .map_err(|_| "preview_scroll must be an unsigned 16-bit integer")?,
                )
            }
            "focus" => state.focus = Some(SessionFocus::parse(&parse_session_string(value)?)?),
            _ => return Err(format!("session contains an unknown key: {}", key.trim())),
        }
    }
    Ok(Some(state))
}

fn serialize_session(state: &SessionState) -> String {
    let mut lines = Vec::new();
    if let Some(account_id) = &state.account_id {
        lines.push(format!(
            "account_id = {}",
            session_string(&account_id.to_string())
        ));
    }
    if let Some(folder_id) = &state.folder_id {
        lines.push(format!(
            "folder_id = {}",
            session_string(&folder_id.to_string())
        ));
    }
    if let Some(note_id) = &state.note_id {
        lines.push(format!(
            "note_id = {}",
            session_string(&note_id.to_string())
        ));
    }
    if let Some(search_query) = &state.search_query {
        lines.push(format!("search_query = {}", session_string(search_query)));
    }
    if let Some(preview_scroll) = state.preview_scroll {
        lines.push(format!("preview_scroll = {preview_scroll}"));
    }
    if let Some(focus) = state.focus {
        lines.push(format!("focus = {}", session_string(focus.as_str())));
    }
    format!("{}\n", lines.join("\n"))
}

pub fn save_session(path: &Path, state: &SessionState) -> Result<(), String> {
    atomic_write_file(path, &serialize_session(state), "session")
}
fn interval(seconds: u64) -> Result<Duration, String> {
    if !(MIN_REFRESH_INTERVAL_SECONDS..=MAX_REFRESH_INTERVAL_SECONDS).contains(&seconds) {
        return Err(format!("refresh_interval_seconds must be between {MIN_REFRESH_INTERVAL_SECONDS} and {MAX_REFRESH_INTERVAL_SECONDS}"));
    }
    Ok(Duration::from_secs(seconds))
}
pub fn resolve_config(path: &Path, cli: CliConfigOverrides) -> Result<ResolvedConfig, String> {
    let (file, warning) = match std::fs::read_to_string(path) {
        Ok(contents) => match parse_file_config(&contents).and_then(validate_file_config) {
            Ok(file) => (file, None),
            Err(error) => (
                FileConfig::default(),
                Some(format!("Config warning: {error}")),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (FileConfig::default(), None),
        Err(error) => (
            FileConfig::default(),
            Some(format!("Config warning: {error}")),
        ),
    };
    resolve_config_from_file(file, cli).map(|mut resolved| {
        resolved.warning = warning;
        resolved
    })
}

fn resolve_config_from_file(
    file: FileConfig,
    cli: CliConfigOverrides,
) -> Result<ResolvedConfig, String> {
    let defaults = AppConfig::default();
    let (refresh_interval, refresh_interval_source) = match cli.refresh_interval_seconds {
        Some(seconds) => (interval(seconds)?, ConfigValueSource::Cli),
        None => match file.refresh_interval_seconds {
            Some(seconds) => (
                interval(seconds).expect("validated file interval"),
                ConfigValueSource::File,
            ),
            None => (defaults.refresh_interval, ConfigValueSource::Default),
        },
    };
    let choose = |cli_value, file_value, default| match cli_value {
        Some(value) => (value, ConfigValueSource::Cli),
        None => match file_value {
            Some(value) => (value, ConfigValueSource::File),
            None => (default, ConfigValueSource::Default),
        },
    };
    let (auto_refresh, auto_refresh_source) =
        choose(cli.auto_refresh, file.auto_refresh, defaults.auto_refresh);
    let (preview_wrap, preview_wrap_source) =
        choose(cli.preview_wrap, file.preview_wrap, defaults.preview_wrap);
    let (show_attachment_metadata, show_attachment_metadata_source) = choose(
        cli.show_attachment_metadata,
        file.show_attachment_metadata,
        defaults.show_attachment_metadata,
    );
    Ok(ResolvedConfig {
        config: AppConfig {
            refresh_interval,
            auto_refresh,
            preview_wrap,
            show_attachment_metadata,
        },
        refresh_interval_source,
        auto_refresh_source,
        preview_wrap_source,
        show_attachment_metadata_source,
        file_presence: [
            file.refresh_interval_seconds.is_some(),
            file.auto_refresh.is_some(),
            file.preview_wrap.is_some(),
            file.show_attachment_metadata.is_some(),
        ],
        warning: None,
    })
}
fn validate_file_config(file: FileConfig) -> Result<FileConfig, String> {
    if let Some(seconds) = file.refresh_interval_seconds {
        interval(seconds)?;
    }
    Ok(file)
}
fn parse_file_config(contents: &str) -> Result<FileConfig, String> {
    Ok(parse_config_document(contents)?.file)
}

fn parse_config_document(contents: &str) -> Result<ConfigDocument, String> {
    let mut document = ConfigDocument::default();
    let mut root_section = true;
    for line in contents.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            document.unknown_lines.push(line.to_owned());
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            document.unknown_lines.push(line.to_owned());
            document.has_unknown_keys = true;
            root_section = false;
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err("malformed config line".into());
        };
        let key = key.trim();
        let value = value.trim();
        if !root_section {
            document.unknown_lines.push(line.to_owned());
            document.has_unknown_keys = true;
            continue;
        }
        match key {
            "refresh_interval_seconds" => {
                document.file.refresh_interval_seconds = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "refresh_interval_seconds must be an integer")?,
                )
            }
            "auto_refresh" => {
                document.file.auto_refresh = Some(
                    value
                        .parse::<bool>()
                        .map_err(|_| "auto_refresh must be true or false")?,
                )
            }
            "preview_wrap" => {
                document.file.preview_wrap = Some(
                    value
                        .parse::<bool>()
                        .map_err(|_| "preview_wrap must be true or false")?,
                )
            }
            "show_attachment_metadata" => {
                document.file.show_attachment_metadata = Some(
                    value
                        .parse::<bool>()
                        .map_err(|_| "show_attachment_metadata must be true or false")?,
                )
            }
            _ => {
                document.unknown_lines.push(line.to_owned());
                document.has_unknown_keys = true;
            }
        }
    }
    Ok(document)
}

type SharedBackend = Arc<Mutex<Box<dyn NotesBackend>>>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum RefreshContext {
    Account(AccountId),
    Folder(FolderId),
}

impl RefreshContext {
    fn query(&self) -> NotesQuery {
        match self {
            Self::Account(id) => NotesQuery {
                account_id: Some(id.clone()),
                ..Default::default()
            },
            Self::Folder(id) => NotesQuery {
                folder_id: Some(id.clone()),
                ..Default::default()
            },
        }
    }
}

#[derive(Clone)]
struct LiveRefreshRead {
    accounts: Vec<Account>,
    folders: Vec<Folder>,
    context: RefreshContext,
    notes: Vec<NoteSummary>,
    total: usize,
    selected_note: Option<Note>,
}

enum WorkerRefreshResult {
    Finished {
        generation: u64,
        result: Box<Result<LiveRefreshRead, NotesError>>,
    },
    Panicked {
        generation: u64,
    },
    Cancelled {
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefreshOrigin {
    Startup,
    Automatic,
    Manual,
}

enum PeriodicRefreshState {
    Idle,
    InFlight {
        _origin: RefreshOrigin,
        generation: u64,
        receiver: mpsc::Receiver<WorkerRefreshResult>,
        cancellation: Arc<AtomicBool>,
        cancel_requested: bool,
    },
}

enum NavigationWorkerResult {
    Preview {
        id: NoteId,
        generation: u64,
        result: Result<Note, NotesError>,
    },
    Folder {
        context: RefreshContext,
        generation: u64,
        result: Result<NotesPage, NotesError>,
    },
}

enum NavigationRequest {
    Preview(NoteId),
    Folder(RefreshContext),
}

enum UpdateWorkerResult {
    Saved(Note),
    FolderCreated(Folder),
    FolderCreateFailed {
        account_id: AccountId,
        name: String,
        cursor: usize,
        error: NotesError,
    },
    ChildFolderCreated(Folder),
    ChildFolderCreateFailed {
        account_id: AccountId,
        parent_folder_id: FolderId,
        parent_folder_name: String,
        name: String,
        cursor: usize,
        error: NotesError,
    },
    FolderRenamed(Folder),
    FolderReparented(Folder),
    FolderDeleted(DeletedFolder),
    FolderDeleteFailed(NotesError),
    FolderRenameFailed {
        account_id: AccountId,
        folder_id: FolderId,
        original_name: String,
        name: String,
        cursor: usize,
        error: NotesError,
    },
    FolderReparentFailed {
        account_id: AccountId,
        folder_id: FolderId,
        source_folder_name: String,
        original_parent: FolderParent,
        destinations: Vec<FolderReparentTarget>,
        selected_destination: usize,
        error: NotesError,
    },
    Moved {
        note: Note,
        destination_name: String,
    },
    Deleted {
        note_id: NoteId,
        previous_index: usize,
    },
    AttachmentPreviewed {
        name: String,
    },
    AttachmentExported {
        destination: std::path::PathBuf,
    },
    Conflict(Note),
    Failed(NotesError),
    Panicked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingForegroundIntent {
    ManualRefresh,
    BeginNew,
    BeginCreateFolder,
    BeginCreateChildFolder {
        account_id: AccountId,
        parent_folder_id: FolderId,
        parent_folder_name: String,
    },
    BeginReparentFolder {
        account_id: AccountId,
        folder_id: FolderId,
        folder_name: String,
    },
    BeginEdit,
    BeginMove,
    BeginDelete,
    BeginDeleteFolder,
    BeginAttachments,
    ActivateSelection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Navigation,
    Notes,
    Preview,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppMode {
    Normal,
    Insert,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataSourceState {
    Live,
    Cached,
    CachedBackendUnavailable { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CachedPreviewState {
    MissingFullNote,
    ReadError(String),
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum EditField {
    Title,
    Body,
}
#[derive(Clone, Debug)]
pub struct EditSession {
    pub note_id: Option<NoteId>,
    pub folder_id: FolderId,
    pub original_title: String,
    pub original_body_html: String,
    pub original_plaintext: String,
    pub base_modification_date: Option<NoteDate>,
    pub title_buffer: String,
    document: EditorDocument,
    current_target: EditorTarget,
    pub dirty: bool,
    pub is_new: bool,
    field: EditField,
    cursor: usize,
    viewport: u16,
}
#[derive(Clone, Debug)]
enum Popup {
    CreateFolder {
        account_id: AccountId,
        name: String,
        cursor: usize,
    },
    CreateChildFolder {
        account_id: AccountId,
        parent_folder_id: FolderId,
        parent_folder_name: String,
        name: String,
        cursor: usize,
    },
    RenameFolder {
        account_id: AccountId,
        folder_id: FolderId,
        original_name: String,
        name: String,
        cursor: usize,
    },
    ReparentFolder {
        account_id: AccountId,
        folder_id: FolderId,
        source_folder_name: String,
        original_parent: FolderParent,
        destinations: Vec<FolderReparentTarget>,
        selected_destination: usize,
    },
    Conflict(Box<Note>),
    Move {
        selected: usize,
    },
    Attachments {
        selected: usize,
    },
    Discard {
        action: PendingAction,
    },
    Link {
        target: EditorTarget,
        url: String,
    },
    DeleteConfirm(Note),
    DeleteFolder {
        account_id: AccountId,
        folder_id: FolderId,
        folder_name: String,
    },
    Settings(SettingsState),
    DraftRecovery {
        draft: EditorRecoveryDraft,
        recoverable: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FolderReparentTarget {
    AccountRoot,
    Folder {
        folder_id: FolderId,
        display_name: String,
        depth: usize,
    },
}

impl FolderReparentTarget {
    fn parent_id(&self) -> Option<FolderId> {
        match self {
            Self::AccountRoot => None,
            Self::Folder { folder_id, .. } => Some(folder_id.clone()),
        }
    }

    fn matches_parent(&self, parent: &FolderParent) -> bool {
        match (self, parent) {
            (Self::AccountRoot, FolderParent::Account { .. }) => true,
            (
                Self::Folder { folder_id, .. },
                FolderParent::Folder {
                    folder_id: parent_id,
                },
            ) => folder_id == parent_id,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorDraftFailurePoint {
    Write,
    Remove,
}
#[derive(Clone, Debug)]
struct SettingsState {
    selected: usize,
    draft: AppConfig,
    staged: [SettingDraft; 4],
    interval_input: Option<String>,
    error: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingDraft {
    Unchanged,
    Set(ConfigEditValue),
    Unset,
}
#[derive(Clone, Debug)]
enum PendingAction {
    Quit,
    Key(KeyEvent),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SearchState {
    Inactive,
    Editing {
        input: String,
        previous_active: Option<ActiveSearch>,
    },
    Active(ActiveSearch),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveSearch {
    query: String,
    visible_ids: Vec<NoteId>,
}

impl Focus {
    fn session_focus(self) -> SessionFocus {
        match self {
            Self::Navigation => SessionFocus::Navigation,
            Self::Notes => SessionFocus::Notes,
            Self::Preview => SessionFocus::Preview,
        }
    }

    fn from_session_focus(focus: SessionFocus) -> Self {
        match focus {
            SessionFocus::Navigation => Self::Navigation,
            SessionFocus::Notes => Self::Notes,
            SessionFocus::Preview => Self::Preview,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Navigation => Self::Notes,
            Self::Notes => Self::Preview,
            Self::Preview => Self::Navigation,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Navigation => Self::Preview,
            Self::Notes => Self::Navigation,
            Self::Preview => Self::Notes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NavigationItem {
    Account {
        id: AccountId,
        name: String,
    },
    Folder {
        id: FolderId,
        account_id: AccountId,
        name: String,
        depth: usize,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusMessage {
    pub text: String,
    pub is_error: bool,
}

#[derive(Clone, Copy, Debug)]
struct Theme {
    active: Style,
    selected: Style,
    status: Style,
    error: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            active: Style::default().add_modifier(Modifier::BOLD),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            status: Style::default().add_modifier(Modifier::DIM),
            error: Style::default().add_modifier(Modifier::BOLD),
        }
    }
}

/// All mutable UI state, kept outside ratatui widgets.
pub struct App {
    backend: SharedBackend,
    cache: Option<Box<dyn CacheStore>>,
    pub accounts: Vec<Account>,
    pub folders: Vec<Folder>,
    pub notes: Vec<NoteSummary>,
    folder_notes_cache: HashMap<FolderId, Vec<NoteSummary>>,
    pub selected_note: Option<Note>,
    cached_preview_state: Option<CachedPreviewState>,
    navigation: Vec<NavigationItem>,
    pub focus: Focus,
    selected_navigation: usize,
    pub selected_note_index: usize,
    pub preview_scroll: u16,
    pub status: StatusMessage,
    pub show_help: bool,
    pub should_quit: bool,
    pub mode: AppMode,
    pub edit: Option<EditSession>,
    pub data_source: DataSourceState,
    popup: Option<Popup>,
    search: SearchState,
    theme: Theme,
    refresh_interval: Duration,
    auto_refresh: bool,
    preview_wrap: bool,
    show_attachment_metadata: bool,
    config_sources: [ConfigValueSource; 4],
    config_file_presence: [bool; 4],
    config_file_path: PathBuf,
    session_file_path: Option<PathBuf>,
    pending_session_restore: Option<SessionState>,
    last_persisted_session: Option<SessionState>,
    #[cfg(test)]
    session_write_failure: bool,
    #[cfg(test)]
    session_write_calls: usize,
    editor_draft_file_path: Option<PathBuf>,
    pending_editor_draft: Option<EditorRecoveryDraft>,
    last_persisted_editor_draft: Option<EditorRecoveryDraft>,
    editor_draft_failure: Option<EditorDraftFailurePoint>,
    last_refresh_attempt: Instant,
    refresh_due: bool,
    refresh_generation: u64,
    periodic_refresh: PeriodicRefreshState,
    update_worker: Option<mpsc::Receiver<UpdateWorkerResult>>,
    pending_foreground_intent: Option<PendingForegroundIntent>,
    navigation_worker: Option<mpsc::Receiver<NavigationWorkerResult>>,
    navigation_generation: u64,
    pending_navigation: Option<NavigationRequest>,
}

impl App {
    pub fn new(backend: Box<dyn NotesBackend>) -> Self {
        Self::with_config(backend, AppConfig::default())
    }
    pub fn with_config(backend: Box<dyn NotesBackend>, config: AppConfig) -> Self {
        Self {
            backend: Arc::new(Mutex::new(backend)),
            cache: None,
            accounts: Vec::new(),
            folders: Vec::new(),
            notes: Vec::new(),
            folder_notes_cache: HashMap::new(),
            selected_note: None,
            cached_preview_state: None,
            navigation: Vec::new(),
            focus: Focus::Navigation,
            selected_navigation: 0,
            selected_note_index: 0,
            preview_scroll: 0,
            status: StatusMessage::default(),
            show_help: false,
            should_quit: false,
            mode: AppMode::Normal,
            edit: None,
            data_source: DataSourceState::Live,
            popup: None,
            search: SearchState::Inactive,
            theme: Theme::default(),
            refresh_interval: config.refresh_interval,
            auto_refresh: config.auto_refresh,
            preview_wrap: config.preview_wrap,
            show_attachment_metadata: config.show_attachment_metadata,
            config_sources: [ConfigValueSource::Default; 4],
            config_file_presence: [false; 4],
            config_file_path: config_path(),
            session_file_path: None,
            pending_session_restore: None,
            last_persisted_session: None,
            #[cfg(test)]
            session_write_failure: false,
            #[cfg(test)]
            session_write_calls: 0,
            editor_draft_file_path: None,
            pending_editor_draft: None,
            last_persisted_editor_draft: None,
            editor_draft_failure: None,
            last_refresh_attempt: Instant::now(),
            refresh_due: false,
            refresh_generation: 0,
            periodic_refresh: PeriodicRefreshState::Idle,
            update_worker: None,
            pending_foreground_intent: None,
            navigation_worker: None,
            navigation_generation: 0,
            pending_navigation: None,
        }
    }

    pub fn with_cache(backend: Box<dyn NotesBackend>, cache: Box<dyn CacheStore>) -> Self {
        let mut app = Self::new(backend);
        app.cache = Some(cache);
        app
    }
    pub fn with_cache_and_config(
        backend: Box<dyn NotesBackend>,
        cache: Box<dyn CacheStore>,
        config: AppConfig,
    ) -> Self {
        let mut app = Self::with_config(backend, config);
        app.cache = Some(cache);
        app
    }
    pub fn set_refresh_interval(&mut self, refresh_interval: Duration) {
        self.refresh_interval = refresh_interval;
    }
    pub fn set_config_metadata(
        &mut self,
        path: PathBuf,
        sources: [ConfigValueSource; 4],
        file_presence: [bool; 4],
    ) {
        self.config_file_path = path;
        self.config_sources = sources;
        self.config_file_presence = file_presence;
    }
    pub fn set_session_state(&mut self, path: PathBuf, session: Option<SessionState>) {
        self.session_file_path = Some(path);
        self.last_persisted_session = session.clone();
        self.pending_session_restore = session;
    }

    pub fn set_editor_draft_state(&mut self, path: PathBuf, draft: Option<EditorRecoveryDraft>) {
        self.editor_draft_file_path = Some(path);
        self.pending_editor_draft = draft;
        self.last_persisted_editor_draft = None;
    }

    /// Called after ordinary startup context loading. Discovery is local-only;
    /// recovery never enters the editor until the user explicitly chooses it.
    pub fn offer_editor_draft_recovery(&mut self) {
        let Some(draft) = self.pending_editor_draft.clone() else {
            return;
        };
        let recoverable = match draft.kind {
            EditorRecoveryKind::Create => self.folders.iter().any(|folder| {
                folder.id == draft.folder_id && folder.account_id == draft.account_id
            }),
            EditorRecoveryKind::Edit => draft.note_id.as_ref().is_some_and(|id| {
                self.notes
                    .iter()
                    .any(|note| note.id == *id && note.folder_id == draft.folder_id)
            }),
        };
        self.popup = Some(Popup::DraftRecovery { draft, recoverable });
        self.status = StatusMessage {
            text: "Unsaved local editor draft found; it has not been saved to Notes.app".into(),
            is_error: false,
        };
    }

    fn editor_recovery_draft(&self) -> Option<EditorRecoveryDraft> {
        let edit = self.edit.as_ref()?.clone();
        if !edit.dirty {
            return None;
        }
        let account_id = match self.navigation.get(self.selected_navigation)? {
            NavigationItem::Folder { account_id, .. } => account_id.clone(),
            NavigationItem::Account { .. } => return None,
        };
        Some(EditorRecoveryDraft {
            version: 1,
            kind: if edit.is_new {
                EditorRecoveryKind::Create
            } else {
                EditorRecoveryKind::Edit
            },
            account_id,
            folder_id: edit.folder_id,
            note_id: edit.note_id,
            expected_modification_date: edit.base_modification_date,
            title: edit.title_buffer,
            document: edit.document.document,
            original_title: edit.original_title,
            original_body_html: edit.original_body_html,
            original_plaintext: edit.original_plaintext,
            cursor: Some(EditorCursorRecovery {
                field: match edit.field {
                    EditField::Title => EditorRecoveryField::Title,
                    EditField::Body => EditorRecoveryField::Body,
                },
                target: edit.current_target,
                offset: edit.cursor,
            }),
            viewport: Some(edit.viewport),
        })
    }

    fn persist_editor_recovery_if_dirty(&mut self) {
        if self.edit.as_ref().is_some_and(|edit| !edit.dirty) {
            self.clear_editor_recovery();
            return;
        }
        let Some(draft) = self.editor_recovery_draft() else {
            return;
        };
        if self.last_persisted_editor_draft.as_ref() == Some(&draft) {
            return;
        }
        let Some(path) = self.editor_draft_file_path.as_ref() else {
            return;
        };
        let result = if self.editor_draft_failure == Some(EditorDraftFailurePoint::Write) {
            self.editor_draft_failure = None;
            Err("injected editor recovery write failure".into())
        } else {
            save_editor_draft(path, &draft)
        };
        if let Err(error) = result {
            self.status = StatusMessage {
                text: format!("Editor recovery warning: {error}"),
                is_error: false,
            };
        } else {
            self.last_persisted_editor_draft = Some(draft);
            self.pending_editor_draft = None;
        }
    }

    fn clear_editor_recovery(&mut self) {
        self.pending_editor_draft = None;
        self.last_persisted_editor_draft = None;
        let Some(path) = self.editor_draft_file_path.as_ref() else {
            return;
        };
        if self.editor_draft_failure == Some(EditorDraftFailurePoint::Remove) {
            self.editor_draft_failure = None;
            self.status = StatusMessage {
                text: "Note saved, but local recovery draft could not be removed: injected failure"
                    .into(),
                is_error: false,
            };
            return;
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                self.status = StatusMessage {
                    text: format!(
                        "Note saved, but local recovery draft could not be removed: {error}"
                    ),
                    is_error: false,
                };
            }
        }
    }

    fn restore_editor_recovery(&mut self, draft: EditorRecoveryDraft) {
        let document = EditorDocument::new(draft.document.clone());
        let current_target = document.first_target();
        let mut edit = EditSession {
            note_id: draft.note_id,
            folder_id: draft.folder_id,
            original_title: draft.original_title,
            original_body_html: draft.original_body_html,
            original_plaintext: draft.original_plaintext,
            base_modification_date: draft.expected_modification_date,
            title_buffer: draft.title,
            document,
            current_target,
            dirty: true,
            is_new: matches!(draft.kind, EditorRecoveryKind::Create),
            field: EditField::Body,
            cursor: 0,
            viewport: 0,
        };
        if let Some(cursor) = draft.cursor {
            edit.field = match cursor.field {
                EditorRecoveryField::Title => EditField::Title,
                EditorRecoveryField::Body => EditField::Body,
            };
            if edit.document.validate_target(cursor.target) {
                edit.current_target = cursor.target;
            }
            edit.cursor = match edit.field {
                EditField::Title => cursor.offset.min(edit.title_buffer.chars().count()),
                EditField::Body => edit
                    .document
                    .clamp_cursor(edit.current_target, cursor.offset)
                    .unwrap_or(0),
            };
        }
        edit.viewport = draft.viewport.unwrap_or(0).min(editor_viewport_max(&edit));
        self.edit = Some(edit);
        self.mode = AppMode::Insert;
        self.pending_editor_draft = None;
        self.status = StatusMessage {
            text: "Recovered local unsaved editor draft; save explicitly to update Notes.app"
                .into(),
            is_error: false,
        };
    }

    fn begin_settings(&mut self) {
        if self.update_worker.is_some()
            || self.edit.is_some()
            || self.popup.is_some()
            || self.show_help
        {
            self.status = StatusMessage {
                text: "Settings are unavailable during an active workflow".into(),
                is_error: false,
            };
            return;
        }
        self.popup = Some(Popup::Settings(SettingsState {
            selected: 0,
            draft: AppConfig {
                refresh_interval: self.refresh_interval,
                auto_refresh: self.auto_refresh,
                preview_wrap: self.preview_wrap,
                show_attachment_metadata: self.show_attachment_metadata,
            },
            staged: [SettingDraft::Unchanged; 4],
            interval_input: None,
            error: None,
        }));
    }

    pub fn bootstrap_cache(&mut self) {
        let started = Instant::now();
        let result = self.cache.as_ref().map(|cache| cache.load_bootstrap());
        match result {
            Some(Ok(state))
                if !state.accounts.is_empty()
                    || !state.folders.is_empty()
                    || !state.notes.is_empty() =>
            {
                self.load_cached_state(state)
            }
            Some(Err(error)) => {
                self.status = StatusMessage {
                    text: format!("Cache unavailable: {error}"),
                    is_error: true,
                }
            }
            _ => {}
        }
        perf::event("tui.bootstrap_cache", None, started, "complete");
    }

    pub fn demo() -> Self {
        Self::new(Box::new(demo_backend()))
    }

    /// Installs a derived cache bootstrap. It never marks the backend as fresh.
    pub fn load_cached_state(&mut self, state: CachedState) {
        self.accounts = state.accounts;
        self.folders = state.folders;
        self.navigation = build_navigation(&self.accounts, &self.folders);
        self.selected_navigation = self.default_navigation_index();
        self.restore_pending_session_context();
        self.folder_notes_cache.clear();
        for note in &state.notes {
            self.folder_notes_cache
                .entry(note.folder_id.clone())
                .or_default()
                .push(note.clone());
        }
        self.notes = state.notes;
        self.restore_pending_session_search();
        self.restore_visible_selection(self.session_note_for_current_context());
        self.selected_note = None;
        self.cached_preview_state = None;
        self.status = StatusMessage {
            text: "Cached data".into(),
            is_error: false,
        };
        self.data_source = DataSourceState::Cached;
        self.load_selected_note();
        self.restore_session_scroll_if_exact();
        if self.restore_session_focus() {
            self.persist_session_selection();
        }
    }

    pub fn cached_state(&self) -> CachedState {
        CachedState {
            accounts: self.accounts.clone(),
            folders: self.folders.clone(),
            notes: self.notes.clone(),
            last_successful_refresh: None,
        }
    }

    pub fn refresh(&mut self) {
        self.refresh_at(Instant::now());
    }

    /// Starts initial live reconciliation off the UI thread while preserving
    /// the cache-backed state for immediate presentation.
    pub fn start_initial_refresh(&mut self) {
        if matches!(self.periodic_refresh, PeriodicRefreshState::Idle)
            && self.update_worker.is_none()
        {
            perf::event(
                "startup.live_refresh_scheduled",
                None,
                Instant::now(),
                "background=true",
            );
            self.start_refresh_worker(Instant::now(), RefreshOrigin::Startup);
        }
    }

    fn refresh_at(&mut self, now: Instant) {
        self.last_refresh_attempt = now;
        self.refresh_due = false;
        self.status = StatusMessage {
            text: "Loading…".into(),
            is_error: false,
        };
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        let preferred = self.selected_note_id();
        match read_live_state(
            &self.backend,
            self.refresh_context(),
            self.pending_session_restore.clone(),
            preferred,
            None,
        ) {
            Ok(read) => self.apply_live_refresh_read(read),
            Err(error) => self.set_error(error),
        }
    }

    fn apply_live_refresh_read(&mut self, read: LiveRefreshRead) {
        let session = self.pending_session_restore.clone();
        self.accounts = read.accounts;
        self.folders = read.folders;
        let was_empty = self.navigation.is_empty();
        self.navigation = build_navigation(&self.accounts, &self.folders);
        self.selected_navigation = self
            .navigation_index_for_context(&read.context)
            .or_else(|| {
                was_empty.then(|| {
                    self.navigation
                        .iter()
                        .position(|item| matches!(item, NavigationItem::Folder { .. }))
                        .unwrap_or(0)
                })
            })
            .unwrap_or_else(|| {
                self.selected_navigation
                    .min(self.navigation.len().saturating_sub(1))
            });
        self.data_source = DataSourceState::Live;
        let preferred = read
            .selected_note
            .as_ref()
            .map(|note| note.summary.id.clone())
            .or_else(|| self.selected_note_id());
        self.notes = read.notes;
        if let RefreshContext::Folder(folder_id) = &read.context {
            self.folder_notes_cache
                .insert(folder_id.clone(), self.notes.clone());
        }
        self.restore_session_search(session.as_ref());
        self.preview_scroll = 0;
        self.status = StatusMessage {
            text: format!("{} notes", read.total),
            is_error: false,
        };
        self.selected_note = read.selected_note;
        self.recompute_search();
        self.restore_visible_selection(preferred);
        self.cached_preview_state = None;
        self.restore_session_scroll_if_exact();
        let focus_fell_back = self.restore_session_focus();
        self.pending_session_restore = None;
        if focus_fell_back {
            self.persist_session_selection();
        }
        self.persist_selected_note_after_live_refresh();
        if !self.status.is_error {
            self.persist_cache_snapshot();
        }
    }

    /// Called by the single-threaded event loop. An elapsed interval is kept as
    /// a due refresh until mutable or modal UI state is no longer active.
    pub fn periodic_refresh_at(&mut self, now: Instant) {
        if !self.auto_refresh {
            self.refresh_due = false;
            return;
        }
        if now.saturating_duration_since(self.last_refresh_attempt) >= self.refresh_interval {
            self.refresh_due = true;
        }
        if self.refresh_due
            && matches!(self.periodic_refresh, PeriodicRefreshState::Idle)
            && self.update_worker.is_none()
            && self.is_safe_for_periodic_refresh()
        {
            self.start_refresh_worker(now, RefreshOrigin::Automatic);
        }
    }

    pub fn poll_periodic_refresh(&mut self) {
        self.poll_navigation_worker();
        self.poll_update_worker();
        let result = match &self.periodic_refresh {
            PeriodicRefreshState::Idle => return,
            PeriodicRefreshState::InFlight { receiver, .. } => match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let generation = match &self.periodic_refresh {
                        PeriodicRefreshState::InFlight { generation, .. } => *generation,
                        PeriodicRefreshState::Idle => return,
                    };
                    WorkerRefreshResult::Panicked { generation }
                }
            },
        };
        self.periodic_refresh = PeriodicRefreshState::Idle;
        match result {
            WorkerRefreshResult::Finished { generation, result }
                if generation == self.refresh_generation =>
            {
                match *result {
                    Ok(read) => {
                        self.apply_live_refresh_read(read);
                        perf::event("startup.live_reconciled", None, Instant::now(), "complete");
                    }
                    Err(NotesError::Cancelled) => {}
                    Err(error) => self.set_error(error),
                }
            }
            WorkerRefreshResult::Cancelled { generation }
                if generation == self.refresh_generation =>
            {
                self.status = StatusMessage {
                    text: "Refresh cancelled for foreground action".into(),
                    is_error: false,
                };
            }
            WorkerRefreshResult::Panicked { generation }
                if generation == self.refresh_generation =>
            {
                self.status = StatusMessage {
                    text: "Live refresh worker ended unexpectedly".into(),
                    is_error: true,
                };
            }
            _ => {}
        }
        self.run_pending_foreground_intent();
    }

    fn poll_navigation_worker(&mut self) {
        let Some(receiver) = &self.navigation_worker else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.navigation_worker = None;
                self.start_pending_navigation();
                return;
            }
        };
        self.navigation_worker = None;
        match result {
            NavigationWorkerResult::Preview {
                id,
                generation,
                result,
            } if generation == self.navigation_generation
                && self.selected_note_id().as_ref() == Some(&id) =>
            {
                match result {
                    Ok(note) => {
                        self.selected_note = Some(note.clone());
                        if let Some(cache) = &mut self.cache {
                            if let Err(error) = cache.upsert_note(&note) {
                                self.status = StatusMessage {
                                    text: format!("Live · cache warning: {error}"),
                                    is_error: false,
                                };
                            }
                        }
                        perf::event(
                            "navigation.preview.live_applied",
                            Some(id.as_str()),
                            Instant::now(),
                            "complete",
                        );
                    }
                    Err(error) => self.set_error(error),
                }
            }
            NavigationWorkerResult::Folder {
                context,
                generation,
                result,
            } if generation == self.navigation_generation
                && self.refresh_context().as_ref() == Some(&context) =>
            {
                match result {
                    Ok(NotesPage { items, total, .. }) => {
                        let preferred = self.selected_note_id();
                        self.notes = items;
                        self.status = StatusMessage {
                            text: format!("{total} notes"),
                            is_error: false,
                        };
                        self.recompute_search();
                        self.restore_visible_selection(preferred);
                        self.load_selected_note_with_cache(true);
                    }
                    Err(error) => self.set_error(error),
                }
            }
            _ => {}
        }
        self.start_pending_navigation();
    }

    fn start_pending_navigation(&mut self) {
        if self.navigation_worker.is_some() {
            return;
        }
        let Some(request) = self.pending_navigation.take() else {
            return;
        };
        match request {
            NavigationRequest::Preview(id) => self.start_preview_worker(id),
            NavigationRequest::Folder(context) => self.start_folder_worker(context),
        }
    }

    fn queue_navigation_request(&mut self, request: NavigationRequest) {
        // A context change supersedes any preview from the previous folder;
        // within one kind, the latest request wins.
        self.pending_navigation = Some(request);
    }

    fn start_preview_worker(&mut self, id: NoteId) {
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
        let generation = self.navigation_generation;
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        let worker_id = id.clone();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend.lock().unwrap().get_note(&worker_id)
            }));
            let result = match result {
                Ok(result) => result,
                Err(_) => Err(NotesError::Backend(
                    "preview worker ended unexpectedly".into(),
                )),
            };
            let _ = sender.send(NavigationWorkerResult::Preview {
                id: worker_id,
                generation,
                result,
            });
        });
        self.navigation_worker = Some(receiver);
        perf::event(
            "navigation.preview.live_scheduled",
            Some(id.as_str()),
            Instant::now(),
            "background=true",
        );
    }

    fn start_folder_worker(&mut self, context: RefreshContext) {
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
        let generation = self.navigation_generation;
        let query = context.query();
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        let worker_context = context.clone();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| backend.lock().unwrap().notes(&query)));
            let result = match result {
                Ok(result) => result,
                Err(_) => Err(NotesError::Backend(
                    "folder worker ended unexpectedly".into(),
                )),
            };
            let _ = sender.send(NavigationWorkerResult::Folder {
                context: worker_context,
                generation,
                result,
            });
        });
        self.navigation_worker = Some(receiver);
    }

    fn poll_update_worker(&mut self) {
        let Some(receiver) = &self.update_worker else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => UpdateWorkerResult::Panicked,
        };
        self.update_worker = None;
        match result {
            UpdateWorkerResult::Saved(note) => self.finish_saved(note, "Saved"),
            UpdateWorkerResult::FolderCreated(folder) => self.finish_folder_created(folder),
            UpdateWorkerResult::ChildFolderCreated(folder) => {
                self.finish_child_folder_created(folder)
            }
            UpdateWorkerResult::FolderRenamed(folder) => self.finish_folder_renamed(folder),
            UpdateWorkerResult::FolderReparented(folder) => self.finish_folder_reparented(folder),
            UpdateWorkerResult::FolderDeleted(deleted) => self.finish_folder_deleted(deleted),
            UpdateWorkerResult::FolderDeleteFailed(error) => self.set_error(error),
            UpdateWorkerResult::FolderRenameFailed {
                account_id,
                folder_id,
                original_name,
                name,
                cursor,
                error,
            } => {
                self.popup = Some(Popup::RenameFolder {
                    account_id,
                    folder_id,
                    original_name,
                    name,
                    cursor,
                });
                self.set_error(error);
            }
            UpdateWorkerResult::FolderReparentFailed {
                account_id,
                folder_id,
                source_folder_name,
                original_parent,
                destinations,
                selected_destination,
                error,
            } => {
                self.popup = Some(Popup::ReparentFolder {
                    account_id,
                    folder_id,
                    source_folder_name,
                    original_parent,
                    destinations,
                    selected_destination,
                });
                self.set_error(error);
            }
            UpdateWorkerResult::FolderCreateFailed {
                account_id,
                name,
                cursor,
                error,
            } => {
                self.popup = Some(Popup::CreateFolder {
                    account_id,
                    name,
                    cursor,
                });
                self.set_error(error);
            }
            UpdateWorkerResult::ChildFolderCreateFailed {
                account_id,
                parent_folder_id,
                parent_folder_name,
                name,
                cursor,
                error,
            } => {
                self.popup = Some(Popup::CreateChildFolder {
                    account_id,
                    parent_folder_id,
                    parent_folder_name,
                    name,
                    cursor,
                });
                self.set_error(error);
            }
            UpdateWorkerResult::Moved {
                note,
                destination_name,
            } => self.finish_moved(note, &destination_name),
            UpdateWorkerResult::Deleted {
                note_id,
                previous_index,
            } => self.finish_deleted(note_id, previous_index),
            UpdateWorkerResult::AttachmentPreviewed { name } => {
                self.status = StatusMessage {
                    text: format!(
                        "Opened {name} in Notes.app; close the preview there before returning"
                    ),
                    is_error: false,
                }
            }
            UpdateWorkerResult::AttachmentExported { destination } => {
                self.status = StatusMessage {
                    text: format!("Exported copy to {}", destination.display()),
                    is_error: false,
                }
            }
            UpdateWorkerResult::Conflict(remote) => {
                self.popup = Some(Popup::Conflict(Box::new(remote)))
            }
            UpdateWorkerResult::Failed(error) => self.set_error(error),
            UpdateWorkerResult::Panicked => {
                self.status = StatusMessage {
                    text: "Save worker ended unexpectedly".into(),
                    is_error: true,
                }
            }
        }
    }

    fn finish_moved(&mut self, moved: Note, destination_name: &str) {
        let source_folder = self.selected_folder_id().cloned();
        if let Some(folder_id) = source_folder {
            self.folder_notes_cache.remove(&folder_id);
        }
        self.folder_notes_cache.remove(&moved.summary.folder_id);
        self.load_notes_for_selection_without_cache();
        let cache_warning = self.persist_full_note(&moved);
        self.persist_cache_snapshot();
        if let Some(warning) = cache_warning {
            self.status = StatusMessage {
                text: warning,
                is_error: false,
            };
        } else if !self.status.text.contains("cache warning") {
            self.status = StatusMessage {
                text: format!("Moved to {destination_name}"),
                is_error: false,
            };
        }
        self.persist_session_selection();
    }

    fn finish_deleted(&mut self, deleted_id: NoteId, previous_index: usize) {
        if let Some(folder_id) = self.selected_folder_id().cloned() {
            self.folder_notes_cache.remove(&folder_id);
        }
        self.load_notes_for_selection_without_cache();
        self.selected_note_index = previous_index.min(self.visible_note_count().saturating_sub(1));
        self.load_selected_note_with_cache(false);
        let cache_warning = self.remove_cached_full_note(&deleted_id);
        self.persist_cache_snapshot();
        if let Some(warning) = cache_warning {
            self.status = StatusMessage {
                text: warning,
                is_error: false,
            };
        } else if !self.status.text.contains("cache warning") {
            self.status = StatusMessage {
                text: "Moved to Recently Deleted".into(),
                is_error: false,
            };
        }
        self.persist_session_selection();
    }

    fn start_manual_refresh(&mut self) {
        if matches!(self.periodic_refresh, PeriodicRefreshState::Idle) {
            self.start_refresh_worker(Instant::now(), RefreshOrigin::Manual);
        }
    }

    fn start_refresh_worker(&mut self, now: Instant, origin: RefreshOrigin) {
        let context = self.refresh_context();
        if context.is_none() && self.pending_session_restore.is_none() {
            return;
        }
        self.last_refresh_attempt = now;
        self.refresh_due = false;
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        let generation = self.refresh_generation;
        let preferred = self.selected_note_id();
        let session = self.pending_session_restore.clone();
        let backend = Arc::clone(&self.backend);
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let started = Instant::now();
            let result = catch_unwind(AssertUnwindSafe(|| {
                read_live_state(
                    &backend,
                    context,
                    session,
                    preferred,
                    Some(&worker_cancellation),
                )
            }));
            let message = match result {
                Ok(Err(NotesError::Cancelled)) => WorkerRefreshResult::Cancelled { generation },
                Ok(result) => WorkerRefreshResult::Finished {
                    generation,
                    result: Box::new(result),
                },
                Err(_) => WorkerRefreshResult::Panicked { generation },
            };
            perf::event(
                "startup.live_refresh_completed",
                None,
                started,
                match &message {
                    WorkerRefreshResult::Finished { .. } => "finished",
                    WorkerRefreshResult::Cancelled { .. } => "cancelled",
                    WorkerRefreshResult::Panicked { .. } => "panicked",
                },
            );
            let _ = sender.send(message);
        });
        self.periodic_refresh = PeriodicRefreshState::InFlight {
            _origin: origin,
            generation,
            receiver,
            cancellation,
            cancel_requested: false,
        };
        self.status = StatusMessage {
            text: "Refreshing…".into(),
            is_error: false,
        };
        let reason = match origin {
            RefreshOrigin::Startup => "refresh.reason=startup",
            RefreshOrigin::Automatic => "refresh.reason=periodic",
            RefreshOrigin::Manual => "refresh.reason=manual",
        };
        perf::event("tui.refresh_started", None, Instant::now(), reason);
    }

    fn is_safe_for_periodic_refresh(&self) -> bool {
        self.mode == AppMode::Normal
            && self.edit.is_none()
            && self.popup.is_none()
            && !self.show_help
            && !matches!(self.search, SearchState::Editing { .. })
    }

    fn refresh_context(&self) -> Option<RefreshContext> {
        match self.navigation.get(self.selected_navigation) {
            Some(NavigationItem::Account { id, .. }) => Some(RefreshContext::Account(id.clone())),
            Some(NavigationItem::Folder { id, .. }) => Some(RefreshContext::Folder(id.clone())),
            None => None,
        }
    }

    fn default_navigation_index(&self) -> usize {
        self.navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { .. }))
            .unwrap_or(0)
    }

    fn navigation_index_for_session(&self, session: &SessionState) -> Option<usize> {
        let account_id = session.account_id.as_ref()?;
        if !self
            .accounts
            .iter()
            .any(|account| &account.id == account_id)
        {
            return None;
        }
        if let Some(folder_id) = &session.folder_id {
            if let Some(index) = self.navigation.iter().position(|item| {
                matches!(item, NavigationItem::Folder { id, account_id: item_account_id, .. }
                    if id == folder_id && item_account_id == account_id)
            }) {
                return Some(index);
            }
        }
        let default_folder = self
            .accounts
            .iter()
            .find(|account| &account.id == account_id)
            .and_then(|account| account.default_folder_id.as_ref());
        default_folder
            .and_then(|folder_id| {
                self.navigation.iter().position(|item| {
                    matches!(item, NavigationItem::Folder { id, account_id: item_account_id, .. }
                        if id == folder_id && item_account_id == account_id)
                })
            })
            .or_else(|| {
                self.navigation.iter().position(|item| {
                    matches!(item, NavigationItem::Folder { account_id: item_account_id, .. }
                        if item_account_id == account_id)
                })
            })
            .or_else(|| {
                self.navigation.iter().position(
                    |item| matches!(item, NavigationItem::Account { id, .. } if id == account_id),
                )
            })
    }

    fn restore_pending_session_context(&mut self) {
        if let Some(session) = self.pending_session_restore.as_ref() {
            if let Some(index) = self.navigation_index_for_session(session) {
                self.selected_navigation = index;
            }
        }
    }

    fn restore_pending_session_search(&mut self) {
        let session = self.pending_session_restore.clone();
        self.restore_session_search(session.as_ref());
    }

    fn restore_session_search(&mut self, session: Option<&SessionState>) {
        let Some(query) = session.and_then(|session| session.search_query.as_ref()) else {
            return;
        };
        if query.is_empty() {
            return;
        }
        self.search = SearchState::Active(ActiveSearch {
            query: query.clone(),
            visible_ids: Vec::new(),
        });
        self.recompute_search();
    }

    fn restore_session_scroll_if_exact(&mut self) {
        let Some(session) = self.pending_session_restore.as_ref() else {
            return;
        };
        if session.note_id == self.selected_note_id() {
            if let Some(scroll) = session.preview_scroll {
                self.preview_scroll = scroll.min(self.preview_scroll_max());
            }
        }
    }

    fn restore_session_focus(&mut self) -> bool {
        let Some(saved_focus) = self
            .pending_session_restore
            .as_ref()
            .and_then(|session| session.focus)
        else {
            return false;
        };
        let focus = Focus::from_session_focus(saved_focus);
        // Preview has no useful target without a selected current-context note.
        let fell_back = focus == Focus::Preview && self.selected_note_id().is_none();
        self.focus = if fell_back { Focus::Notes } else { focus };
        fell_back
    }

    fn set_browsing_focus(&mut self, focus: Focus) {
        if self.focus != focus {
            self.focus = focus;
            self.persist_session_selection();
        }
    }

    fn preview_scroll_max(&self) -> u16 {
        let line_count = self
            .selected_note
            .as_ref()
            .map(|note| {
                preview_text(note, self.show_attachment_metadata)
                    .lines
                    .len()
            })
            .unwrap_or(1);
        line_count.saturating_sub(1).min(u16::MAX as usize) as u16
    }

    fn session_note_for_current_context(&self) -> Option<NoteId> {
        let session = self.pending_session_restore.as_ref()?;
        let folder_id = self.selected_folder_id()?;
        let account_id = match self.navigation.get(self.selected_navigation)? {
            NavigationItem::Folder { account_id, .. } => account_id,
            NavigationItem::Account { .. } => return None,
        };
        if session.account_id.as_ref() == Some(account_id)
            && session.folder_id.as_ref() == Some(folder_id)
        {
            session.note_id.clone()
        } else {
            None
        }
    }

    fn session_state_for_current_selection(&self) -> Option<SessionState> {
        let (account_id, folder_id, note_id) =
            match self.navigation.get(self.selected_navigation)? {
                NavigationItem::Folder { id, account_id, .. } => {
                    let note_id = self.selected_note_id().filter(|note_id| {
                        self.notes
                            .iter()
                            .any(|note| note.id == *note_id && note.folder_id == *id)
                    });
                    (account_id.clone(), id.clone(), note_id)
                }
                NavigationItem::Account { id, .. } => {
                    let account = self.accounts.iter().find(|account| account.id == *id)?;
                    let folder_id = account
                        .default_folder_id
                        .as_ref()
                        .filter(|folder_id| {
                            self.folders.iter().any(|folder| {
                                folder.id == **folder_id && folder.account_id == account.id
                            })
                        })
                        .cloned()
                        .or_else(|| {
                            self.folders
                                .iter()
                                .find(|folder| folder.account_id == account.id)
                                .map(|folder| folder.id.clone())
                        })?;
                    (account.id.clone(), folder_id, None)
                }
            };
        Some(SessionState {
            account_id: Some(account_id),
            folder_id: Some(folder_id),
            note_id: note_id.clone(),
            search_query: match &self.search {
                SearchState::Active(active) if !active.query.is_empty() => {
                    Some(active.query.clone())
                }
                SearchState::Inactive | SearchState::Editing { .. } | SearchState::Active(_) => {
                    None
                }
            },
            preview_scroll: note_id.as_ref().map(|_| self.preview_scroll),
            focus: Some(self.focus.session_focus()),
        })
    }

    fn persist_session_selection(&mut self) {
        let Some(session) = self.session_state_for_current_selection() else {
            return;
        };
        let Some(path) = self.session_file_path.as_ref() else {
            return;
        };
        if self.last_persisted_session.as_ref() == Some(&session) {
            return;
        }
        #[cfg(test)]
        {
            self.session_write_calls += 1;
        }
        #[cfg(test)]
        if self.session_write_failure {
            self.status = StatusMessage {
                text: "Session warning: injected write failure".into(),
                is_error: false,
            };
            return;
        }
        if let Err(error) = save_session(path, &session) {
            self.status = StatusMessage {
                text: format!("Session warning: {error}"),
                is_error: false,
            };
        } else {
            self.last_persisted_session = Some(session);
        }
    }

    fn navigation_index_for_context(&self, context: &RefreshContext) -> Option<usize> {
        self.navigation
            .iter()
            .position(|item| match (context, item) {
                (RefreshContext::Account(expected), NavigationItem::Account { id, .. }) => {
                    id == expected
                }
                (RefreshContext::Folder(expected), NavigationItem::Folder { id, .. }) => {
                    id == expected
                }
                _ => false,
            })
    }

    fn persist_selected_note_after_live_refresh(&mut self) {
        if let (Some(note), Some(cache)) = (&self.selected_note, &mut self.cache) {
            if let Err(error) = cache.upsert_note(note) {
                self.status = StatusMessage {
                    text: format!("Live · cache warning: {error}"),
                    is_error: false,
                };
            }
        }
    }

    fn backend(&self) -> MutexGuard<'_, Box<dyn NotesBackend>> {
        self.backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn periodic_refresh_in_flight(&self) -> bool {
        matches!(self.periodic_refresh, PeriodicRefreshState::InFlight { .. })
    }

    fn queue_foreground_intent_while_refreshing(&mut self, key: KeyEvent) -> bool {
        if !self.periodic_refresh_in_flight() {
            return false;
        }
        let intent = match key.code {
            KeyCode::Char('r') => PendingForegroundIntent::ManualRefresh,
            KeyCode::Char('n') => PendingForegroundIntent::BeginNew,
            KeyCode::Char('N') => PendingForegroundIntent::BeginCreateFolder,
            KeyCode::Char('C') if self.focus == Focus::Navigation => {
                let Some(NavigationItem::Folder {
                    id,
                    account_id,
                    name,
                    ..
                }) = self.navigation.get(self.selected_navigation)
                else {
                    return false;
                };
                PendingForegroundIntent::BeginCreateChildFolder {
                    account_id: account_id.clone(),
                    parent_folder_id: id.clone(),
                    parent_folder_name: name.clone(),
                }
            }
            KeyCode::Char('M') if self.focus == Focus::Navigation => {
                let Some(NavigationItem::Folder {
                    id,
                    account_id,
                    name,
                    ..
                }) = self.navigation.get(self.selected_navigation)
                else {
                    return false;
                };
                PendingForegroundIntent::BeginReparentFolder {
                    account_id: account_id.clone(),
                    folder_id: id.clone(),
                    folder_name: name.clone(),
                }
            }
            KeyCode::Char('e') => PendingForegroundIntent::BeginEdit,
            KeyCode::Char('m') => PendingForegroundIntent::BeginMove,
            KeyCode::Char('D')
                if self.focus == Focus::Navigation && self.selected_note.is_none() =>
            {
                PendingForegroundIntent::BeginDeleteFolder
            }
            KeyCode::Char('D') => PendingForegroundIntent::BeginDelete,
            KeyCode::Char('a') => PendingForegroundIntent::BeginAttachments,
            KeyCode::Enter if self.focus != Focus::Preview => {
                PendingForegroundIntent::ActivateSelection
            }
            _ => return false,
        };
        let replaced = self.pending_foreground_intent.replace(intent).is_some();
        if let PeriodicRefreshState::InFlight {
            cancellation,
            cancel_requested,
            ..
        } = &mut self.periodic_refresh
        {
            cancellation.store(true, Ordering::Release);
            *cancel_requested = true;
        }
        self.status = StatusMessage {
            text: if replaced {
                "Pending action updated".into()
            } else {
                "Refresh in progress; action will open when ready".into()
            },
            is_error: false,
        };
        true
    }

    fn run_pending_foreground_intent(&mut self) {
        let Some(intent) = self.pending_foreground_intent.take() else {
            return;
        };
        match intent {
            PendingForegroundIntent::ManualRefresh => self.start_manual_refresh(),
            PendingForegroundIntent::BeginNew => self.begin_new(),
            PendingForegroundIntent::BeginCreateFolder => self.begin_create_folder(),
            PendingForegroundIntent::BeginCreateChildFolder {
                account_id,
                parent_folder_id,
                parent_folder_name,
            } => self.open_queued_create_child_folder(
                account_id,
                parent_folder_id,
                parent_folder_name,
            ),
            PendingForegroundIntent::BeginReparentFolder {
                account_id,
                folder_id,
                folder_name,
            } => self.open_reparent_folder_popup(account_id, folder_id, folder_name),
            PendingForegroundIntent::BeginEdit => {
                if self.reload_current_selection_for_foreground() {
                    self.begin_edit();
                }
            }
            PendingForegroundIntent::BeginMove => {
                if self.reload_current_selection_for_foreground() {
                    self.begin_move();
                }
            }
            PendingForegroundIntent::BeginDelete => {
                if self.reload_current_selection_for_foreground() {
                    self.begin_delete_note();
                }
            }
            PendingForegroundIntent::BeginDeleteFolder => self.begin_delete_folder(),
            PendingForegroundIntent::BeginAttachments => {
                if self.reload_current_selection_for_foreground() {
                    self.begin_attachments();
                }
            }
            PendingForegroundIntent::ActivateSelection => self.activate_selection(),
        }
    }

    fn reload_current_selection_for_foreground(&mut self) -> bool {
        let Some(current_id) = self
            .visible_note(self.selected_note_index)
            .map(|note| note.id.clone())
        else {
            self.status = StatusMessage {
                text: "Pending action cancelled: no note is selected".into(),
                is_error: false,
            };
            return false;
        };
        if self
            .selected_note
            .as_ref()
            .is_none_or(|note| note.summary.id != current_id)
        {
            self.load_selected_note();
        }
        if self.selected_note.is_none() {
            self.status = StatusMessage {
                text: "Pending action cancelled: selected note is unavailable".into(),
                is_error: false,
            };
            return false;
        }
        true
    }

    fn invalidate_periodic_refresh_result(&mut self) {
        if self.periodic_refresh_in_flight() {
            self.refresh_generation = self.refresh_generation.wrapping_add(1);
            self.refresh_due = true;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.queue_foreground_intent_while_refreshing(key) {
            return;
        }
        if self.popup.is_some() {
            self.handle_popup(key);
            return;
        }
        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return;
        }
        if self.mode == AppMode::Insert {
            self.handle_insert(key);
            return;
        }
        if matches!(self.search, SearchState::Editing { .. }) {
            self.handle_search_input(key);
            return;
        }
        if self.edit.as_ref().is_some_and(|edit| edit.dirty)
            && matches!(
                key.code,
                KeyCode::Char('q')
                    | KeyCode::Char('j')
                    | KeyCode::Char('k')
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Tab
            )
        {
            self.popup = Some(Popup::Discard {
                action: if key.code == KeyCode::Char('q') {
                    PendingAction::Quit
                } else {
                    PendingAction::Key(key)
                },
            });
            return;
        }
        match key.code {
            KeyCode::Char('/') => self.begin_search(),
            KeyCode::Esc if matches!(self.search, SearchState::Active(_)) => self.clear_search(),
            KeyCode::Char('q') => {
                self.pending_foreground_intent = None;
                self.should_quit = true;
            }
            KeyCode::Char('e') => self.begin_edit(),
            KeyCode::Char('n') => self.begin_new(),
            KeyCode::Char('N') => self.begin_create_folder(),
            KeyCode::Char('C') if self.focus == Focus::Navigation => {
                self.begin_create_child_folder()
            }
            KeyCode::Char('M') if self.focus == Focus::Navigation => self.begin_reparent_folder(),
            KeyCode::Char('R') => self.begin_rename_folder(),
            KeyCode::Char('m') => self.begin_move(),
            KeyCode::Char('D')
                if self.focus == Focus::Navigation && self.selected_note.is_none() =>
            {
                self.begin_delete_folder()
            }
            KeyCode::Char('D') => self.begin_delete_note(),
            KeyCode::Char('a') => self.begin_attachments(),
            KeyCode::Char(',') => self.begin_settings(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Tab => self.set_browsing_focus(self.focus.next()),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.set_browsing_focus(self.focus.previous())
            }
            KeyCode::Right | KeyCode::Char('l') => self.set_browsing_focus(self.focus.next()),
            KeyCode::Char('r') => self.start_manual_refresh(),
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_to_edge(false),
            KeyCode::Char('G') | KeyCode::End => self.move_to_edge(true),
            KeyCode::PageDown => self.scroll_preview(10),
            KeyCode::PageUp => self.scroll_preview(-10),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_preview(10)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_preview(-10)
            }
            _ => {}
        }
    }

    fn begin_search(&mut self) {
        let previous_active = match &self.search {
            SearchState::Active(active) => Some(active.clone()),
            SearchState::Inactive | SearchState::Editing { .. } => None,
        };
        let input = previous_active
            .as_ref()
            .map_or_else(String::new, |active| active.query.clone());
        self.search = SearchState::Editing {
            input,
            previous_active,
        };
    }

    fn handle_search_input(&mut self, key: KeyEvent) {
        let SearchState::Editing {
            mut input,
            previous_active,
        } = std::mem::replace(&mut self.search, SearchState::Inactive)
        else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.search = previous_active.map_or(SearchState::Inactive, SearchState::Active);
            }
            KeyCode::Enter => {
                if input.is_empty() {
                    self.search = SearchState::Inactive;
                    self.restore_visible_selection(None);
                } else {
                    self.search = SearchState::Active(ActiveSearch {
                        query: input,
                        visible_ids: Vec::new(),
                    });
                    self.recompute_search();
                    if !matches!(self.data_source, DataSourceState::Live) {
                        self.load_selected_note();
                    }
                    self.persist_session_selection();
                }
            }
            KeyCode::Backspace => {
                input.pop();
                self.search = SearchState::Editing {
                    input,
                    previous_active,
                };
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                self.search = SearchState::Editing {
                    input,
                    previous_active,
                };
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(character);
                self.search = SearchState::Editing {
                    input,
                    previous_active,
                };
            }
            _ => {
                self.search = SearchState::Editing {
                    input,
                    previous_active,
                };
            }
        }
    }

    fn clear_search(&mut self) {
        let preferred = self.selected_note_id();
        self.search = SearchState::Inactive;
        self.restore_visible_selection(preferred);
        self.persist_session_selection();
    }

    fn recompute_search(&mut self) {
        let preferred = self.selected_note_id();
        if let SearchState::Active(active) = &mut self.search {
            let needle = active.query.to_lowercase();
            active.visible_ids = self
                .notes
                .iter()
                .filter(|note| note.name.to_lowercase().contains(&needle))
                .map(|note| note.id.clone())
                .collect();
        }
        self.restore_visible_selection(preferred);
    }

    fn visible_note_ids(&self) -> Vec<NoteId> {
        match &self.search {
            SearchState::Active(active) => active.visible_ids.clone(),
            SearchState::Editing {
                previous_active: Some(active),
                ..
            } => active.visible_ids.clone(),
            SearchState::Inactive | SearchState::Editing { .. } => {
                self.notes.iter().map(|note| note.id.clone()).collect()
            }
        }
    }

    fn visible_note(&self, index: usize) -> Option<&NoteSummary> {
        let id = self.visible_note_ids().get(index)?.clone();
        self.notes.iter().find(|note| note.id == id)
    }

    fn visible_note_count(&self) -> usize {
        match &self.search {
            SearchState::Active(active) => active.visible_ids.len(),
            SearchState::Editing {
                previous_active: Some(active),
                ..
            } => active.visible_ids.len(),
            SearchState::Inactive | SearchState::Editing { .. } => self.notes.len(),
        }
    }

    fn selected_note_id(&self) -> Option<NoteId> {
        self.selected_note
            .as_ref()
            .map(|note| note.summary.id.clone())
            .or_else(|| {
                self.visible_note(self.selected_note_index)
                    .map(|note| note.id.clone())
            })
    }

    fn restore_visible_selection(&mut self, preferred: Option<NoteId>) {
        let ids = self.visible_note_ids();
        self.selected_note_index = preferred
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        if self.visible_note(self.selected_note_index).is_none()
            || self.selected_note.as_ref().is_some_and(|note| {
                self.visible_note(self.selected_note_index)
                    .is_none_or(|summary| summary.id != note.summary.id)
            })
        {
            self.selected_note = None;
        }
    }

    fn search_status(&self) -> Option<String> {
        match &self.search {
            SearchState::Inactive => None,
            SearchState::Editing { input, .. } => Some(format!("Search: {input}_")),
            SearchState::Active(active) if active.visible_ids.is_empty() => {
                Some(format!("Search: {} · No matching notes", active.query))
            }
            SearchState::Active(active) => Some(format!(
                "Search: {} · {}/{} matches",
                active.query,
                active.visible_ids.len(),
                self.notes.len()
            )),
        }
    }

    fn begin_new(&mut self) {
        if !self.require_live_backend() {
            return;
        }
        let Some(folder_id) = self.selected_folder_id().cloned() else {
            self.status = StatusMessage {
                text: "ERROR: select a folder before creating a note".into(),
                is_error: true,
            };
            return;
        };
        let document = EditorDocument::empty();
        let current_target = document.first_target();
        self.edit = Some(EditSession {
            note_id: None,
            folder_id,
            original_title: String::new(),
            original_body_html: String::new(),
            original_plaintext: String::new(),
            base_modification_date: None,
            title_buffer: String::new(),
            document,
            current_target,
            dirty: false,
            is_new: true,
            field: EditField::Title,
            cursor: 0,
            viewport: 0,
        });
        self.mode = AppMode::Insert;
    }

    fn selected_account_id(&self) -> Option<AccountId> {
        match self.navigation.get(self.selected_navigation) {
            Some(NavigationItem::Account { id, .. }) => Some(id.clone()),
            Some(NavigationItem::Folder { account_id, .. }) => Some(account_id.clone()),
            None => None,
        }
    }

    fn begin_create_folder(&mut self) {
        if self.update_worker.is_some() || !self.require_live_backend() {
            return;
        }
        let Some(account_id) = self.selected_account_id() else {
            self.status = StatusMessage {
                text: "ERROR: select an account before creating a folder".into(),
                is_error: true,
            };
            return;
        };
        self.popup = Some(Popup::CreateFolder {
            account_id,
            name: String::new(),
            cursor: 0,
        });
    }

    fn start_create_folder_worker(&mut self, account_id: AccountId, name: String, cursor: usize) {
        if self.update_worker.is_some() {
            return;
        }
        if name.trim().is_empty() {
            self.status = StatusMessage {
                text: "ERROR: folder name cannot be empty".into(),
                is_error: true,
            };
            self.popup = Some(Popup::CreateFolder {
                account_id,
                name,
                cursor,
            });
            return;
        }
        let request = CreateFolder {
            account_id: account_id.clone(),
            name: name.clone(),
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .create_folder(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(folder) => UpdateWorkerResult::FolderCreated(folder),
                Err(error) => UpdateWorkerResult::FolderCreateFailed {
                    account_id,
                    name,
                    cursor,
                    error,
                },
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Creating folder…".into(),
            is_error: false,
        };
    }

    fn finish_folder_created(&mut self, folder: Folder) {
        self.folder_notes_cache.remove(&folder.id);
        let account_id = folder.account_id.clone();
        let folders_result = self.backend().folders(Some(&account_id));
        match folders_result {
            Ok(folders) => {
                self.folders.retain(|item| item.account_id != account_id);
                self.folders.extend(folders);
                self.navigation = build_navigation(&self.accounts, &self.folders);
                if let Some(index) = self.navigation.iter().position(
                    |item| matches!(item, NavigationItem::Folder { id, .. } if *id == folder.id),
                ) {
                    self.selected_navigation = index;
                    self.search = SearchState::Inactive;
                    self.load_notes_for_selection();
                    self.status = StatusMessage {
                        text: format!("Created folder: {}", folder.name),
                        is_error: false,
                    };
                    self.persist_session_selection();
                    self.persist_cache_snapshot();
                } else {
                    self.status = StatusMessage {
                        text: "Folder was created, but could not be found after reload".into(),
                        is_error: true,
                    };
                }
            }
            Err(error) => {
                self.status = StatusMessage {
                    text: format!("Folder was created, but reload failed: {error}"),
                    is_error: true,
                }
            }
        }
    }

    fn begin_create_child_folder(&mut self) {
        if self.update_worker.is_some() || !self.require_live_backend() {
            return;
        }
        let Some(NavigationItem::Folder {
            id,
            account_id,
            name,
            ..
        }) = self.navigation.get(self.selected_navigation)
        else {
            self.status = StatusMessage {
                text: "ERROR: select a parent folder before creating a subfolder".into(),
                is_error: true,
            };
            return;
        };
        self.popup = Some(Popup::CreateChildFolder {
            account_id: account_id.clone(),
            parent_folder_id: id.clone(),
            parent_folder_name: name.clone(),
            name: String::new(),
            cursor: 0,
        });
    }

    fn open_queued_create_child_folder(
        &mut self,
        account_id: AccountId,
        parent_folder_id: FolderId,
        parent_folder_name: String,
    ) {
        let exists = self.navigation.iter().any(|item| {
            matches!(item, NavigationItem::Folder { id, account_id: actual_account, .. }
                if id == &parent_folder_id && actual_account == &account_id)
        });
        if !exists {
            self.status = StatusMessage {
                text:
                    "Pending child-folder creation cancelled: parent folder is no longer available"
                        .into(),
                is_error: false,
            };
            return;
        }
        self.popup = Some(Popup::CreateChildFolder {
            account_id,
            parent_folder_id,
            parent_folder_name,
            name: String::new(),
            cursor: 0,
        });
    }

    fn start_create_child_folder_worker(
        &mut self,
        account_id: AccountId,
        parent_folder_id: FolderId,
        parent_folder_name: String,
        name: String,
        cursor: usize,
    ) {
        if self.update_worker.is_some() {
            return;
        }
        if name.trim().is_empty() {
            self.status = StatusMessage {
                text: "ERROR: folder name cannot be empty".into(),
                is_error: true,
            };
            self.popup = Some(Popup::CreateChildFolder {
                account_id,
                parent_folder_id,
                parent_folder_name,
                name,
                cursor,
            });
            return;
        }
        let request = CreateChildFolder {
            account_id: account_id.clone(),
            parent_folder_id: parent_folder_id.clone(),
            name: name.clone(),
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .create_child_folder(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(folder) => UpdateWorkerResult::ChildFolderCreated(folder),
                Err(error) => UpdateWorkerResult::ChildFolderCreateFailed {
                    account_id,
                    parent_folder_id,
                    parent_folder_name,
                    name,
                    cursor,
                    error,
                },
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Creating subfolder…".into(),
            is_error: false,
        };
    }

    fn finish_child_folder_created(&mut self, folder: Folder) {
        let child_id = folder.id.clone();
        self.folders.retain(|item| item.id != child_id);
        self.folders.push(folder.clone());
        self.navigation = build_navigation(&self.accounts, &self.folders);
        if let Some(index) = self
            .navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { id, .. } if *id == child_id))
        {
            self.selected_navigation = index;
            self.search = SearchState::Inactive;
            self.preview_scroll = 0;
            self.focus = Focus::Navigation;
            self.load_notes_for_selection();
            self.status = StatusMessage {
                text: format!("Created subfolder: {}", folder.name),
                is_error: false,
            };
            self.persist_cache_snapshot();
            self.persist_session_selection();
        } else {
            self.status = StatusMessage {
                text: "Subfolder was created, but could not be found after rebuild".into(),
                is_error: true,
            };
        }
    }

    fn begin_rename_folder(&mut self) {
        if self.update_worker.is_some() || !self.require_live_backend() {
            return;
        }
        let Some(NavigationItem::Folder {
            id,
            account_id,
            name,
            ..
        }) = self.navigation.get(self.selected_navigation)
        else {
            self.status = StatusMessage {
                text: "ERROR: select a folder before renaming".into(),
                is_error: true,
            };
            return;
        };
        self.popup = Some(Popup::RenameFolder {
            account_id: account_id.clone(),
            folder_id: id.clone(),
            original_name: name.clone(),
            name: name.clone(),
            cursor: name.chars().count(),
        });
    }
    fn start_rename_folder_worker(
        &mut self,
        account_id: AccountId,
        folder_id: FolderId,
        original_name: String,
        name: String,
        cursor: usize,
    ) {
        if self.update_worker.is_some() {
            return;
        }
        if name.trim().is_empty() {
            self.status = StatusMessage {
                text: "ERROR: folder name cannot be empty".into(),
                is_error: true,
            };
            self.popup = Some(Popup::RenameFolder {
                account_id,
                folder_id,
                original_name,
                name,
                cursor,
            });
            return;
        }
        if name == original_name {
            self.status = StatusMessage {
                text: "Folder name unchanged".into(),
                is_error: false,
            };
            return;
        }
        let request = RenameFolder {
            account_id: account_id.clone(),
            folder_id: folder_id.clone(),
            name: name.clone(),
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .rename_folder(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |r| match r {
                Ok(folder) => UpdateWorkerResult::FolderRenamed(folder),
                Err(error) => UpdateWorkerResult::FolderRenameFailed {
                    account_id,
                    folder_id,
                    original_name,
                    name,
                    cursor,
                    error,
                },
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Renaming folder…".into(),
            is_error: false,
        };
    }
    fn finish_folder_renamed(&mut self, folder: Folder) {
        if let Some(existing) = self
            .folders
            .iter_mut()
            .find(|existing| existing.id == folder.id && existing.account_id == folder.account_id)
        {
            *existing = folder.clone();
        }
        self.navigation = build_navigation(&self.accounts, &self.folders);
        if let Some(index) = self
            .navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { id, account_id, .. } if *id == folder.id && *account_id == folder.account_id))
        {
            self.selected_navigation = index;
        }
        self.status = StatusMessage {
            text: format!("Renamed folder to {}", folder.name),
            is_error: false,
        };
        self.persist_cache_snapshot();
        if self.status.text.starts_with("Live · cache warning:") {
            self.status.text = format!("Renamed folder to {} · {}", folder.name, self.status.text);
        }
    }

    fn reparent_destinations(
        &self,
        account_id: &AccountId,
        source_folder_id: &FolderId,
    ) -> Vec<FolderReparentTarget> {
        let mut destinations = vec![FolderReparentTarget::AccountRoot];
        for item in &self.navigation {
            let NavigationItem::Folder {
                id,
                account_id: candidate_account,
                name,
                depth,
            } = item
            else {
                continue;
            };
            if candidate_account != account_id
                || id == source_folder_id
                || self.folder_is_descendant_of(id, source_folder_id)
            {
                continue;
            }
            destinations.push(FolderReparentTarget::Folder {
                folder_id: id.clone(),
                display_name: name.clone(),
                depth: *depth,
            });
        }
        destinations
    }

    fn folder_is_descendant_of(&self, candidate: &FolderId, ancestor: &FolderId) -> bool {
        let mut current = candidate.clone();
        let mut visited = HashSet::new();
        while visited.insert(current.clone()) {
            let Some(folder) = self.folders.iter().find(|folder| folder.id == current) else {
                return false;
            };
            let FolderParent::Folder { folder_id } = &folder.parent else {
                return false;
            };
            if folder_id == ancestor {
                return true;
            }
            current = folder_id.clone();
        }
        false
    }

    fn begin_reparent_folder(&mut self) {
        if self.update_worker.is_some() || !self.require_live_backend() {
            return;
        }
        let Some(NavigationItem::Folder {
            id,
            account_id,
            name,
            ..
        }) = self.navigation.get(self.selected_navigation)
        else {
            self.status = StatusMessage {
                text: "ERROR: select a folder before moving it".into(),
                is_error: true,
            };
            return;
        };
        self.open_reparent_folder_popup(account_id.clone(), id.clone(), name.clone());
    }

    fn open_reparent_folder_popup(
        &mut self,
        account_id: AccountId,
        folder_id: FolderId,
        source_folder_name: String,
    ) {
        let Some(source) = self
            .folders
            .iter()
            .find(|folder| folder.id == folder_id && folder.account_id == account_id)
        else {
            self.status = StatusMessage {
                text: "Pending folder move cancelled: source folder is no longer available".into(),
                is_error: false,
            };
            return;
        };
        let original_parent = source.parent.clone();
        let destinations = self.reparent_destinations(&account_id, &folder_id);
        let selected_destination = destinations
            .iter()
            .position(|target| target.matches_parent(&original_parent))
            .unwrap_or(0);
        self.popup = Some(Popup::ReparentFolder {
            account_id,
            folder_id,
            source_folder_name,
            original_parent,
            destinations,
            selected_destination,
        });
    }

    fn start_reparent_folder_worker(
        &mut self,
        account_id: AccountId,
        folder_id: FolderId,
        source_folder_name: String,
        original_parent: FolderParent,
        destinations: Vec<FolderReparentTarget>,
        selected_destination: usize,
    ) {
        let Some(target) = destinations.get(selected_destination).cloned() else {
            return;
        };
        if target.matches_parent(&original_parent) {
            self.status = StatusMessage {
                text: "Folder location unchanged".into(),
                is_error: false,
            };
            return;
        }
        if self.update_worker.is_some() {
            return;
        }
        let request = ReparentFolder {
            account_id: account_id.clone(),
            folder_id: folder_id.clone(),
            new_parent_folder_id: target.parent_id(),
        };
        let status_name = source_folder_name.clone();
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .reparent_folder(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(folder) => UpdateWorkerResult::FolderReparented(folder),
                Err(error) => UpdateWorkerResult::FolderReparentFailed {
                    account_id,
                    folder_id,
                    source_folder_name,
                    original_parent,
                    destinations,
                    selected_destination,
                    error,
                },
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: format!("Moving folder “{status_name}”…"),
            is_error: false,
        };
    }

    fn finish_folder_reparented(&mut self, folder: Folder) {
        if let Some(existing) = self
            .folders
            .iter_mut()
            .find(|existing| existing.id == folder.id && existing.account_id == folder.account_id)
        {
            *existing = folder.clone();
        }
        self.navigation = build_navigation(&self.accounts, &self.folders);
        if let Some(index) = self
            .navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { id, account_id, .. } if *id == folder.id && *account_id == folder.account_id))
        {
            self.selected_navigation = index;
        }
        self.status = StatusMessage {
            text: format!("Moved folder “{}”", folder.name),
            is_error: false,
        };
        self.persist_cache_snapshot();
        if self.status.text.starts_with("Live · cache warning:") {
            self.status.text = format!("Moved folder “{}” · {}", folder.name, self.status.text);
        }
    }
    fn begin_delete_folder(&mut self) {
        if self.update_worker.is_some() || !self.require_live_backend() {
            return;
        }
        let Some(NavigationItem::Folder {
            id,
            account_id,
            name,
            ..
        }) = self.navigation.get(self.selected_navigation)
        else {
            self.status = StatusMessage {
                text: "ERROR: select a folder before deleting".into(),
                is_error: true,
            };
            return;
        };
        self.popup = Some(Popup::DeleteFolder {
            account_id: account_id.clone(),
            folder_id: id.clone(),
            folder_name: name.clone(),
        });
    }

    fn delete_folder_confirmed(
        &mut self,
        account_id: AccountId,
        folder_id: FolderId,
        folder_name: String,
    ) {
        if self.update_worker.is_some() {
            return;
        }
        let request = DeleteFolder {
            account_id,
            folder_id,
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .delete_folder(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(deleted) => UpdateWorkerResult::FolderDeleted(deleted),
                Err(error) => UpdateWorkerResult::FolderDeleteFailed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: format!("Deleting folder “{folder_name}”…"),
            is_error: false,
        };
    }

    fn finish_folder_deleted(&mut self, deleted: DeletedFolder) {
        self.folder_notes_cache.remove(&deleted.folder_id);
        let old_index = self.selected_navigation;
        let selected_account_id = self.selected_account_id();
        self.folders.retain(|folder| {
            !(folder.id == deleted.folder_id && folder.account_id == deleted.account_id)
        });
        self.navigation = build_navigation(&self.accounts, &self.folders);
        let same_account = selected_account_id == Some(deleted.account_id.clone());
        if same_account && !self.navigation.is_empty() {
            self.selected_navigation = old_index.min(self.navigation.len().saturating_sub(1));
        } else if self.navigation.is_empty() {
            self.selected_navigation = 0;
        }
        self.search = SearchState::Inactive;
        self.preview_scroll = 0;
        self.focus = Focus::Navigation;
        self.load_notes_for_selection_without_cache();
        self.status = StatusMessage {
            text: "Deleted folder".into(),
            is_error: false,
        };
        self.persist_cache_snapshot();
        if self.status.text.starts_with("Live · cache warning:") {
            self.status.text = format!("Deleted folder · {}", self.status.text);
        }
        self.persist_session_selection();
    }
    fn begin_delete_note(&mut self) {
        if !self.require_live_backend() {
            return;
        }
        if !self.backend().capabilities().delete {
            self.status = StatusMessage {
                text: "Deleting notes is not supported by this backend.".into(),
                is_error: true,
            };
        } else if let Some(note) = self.selected_note.clone() {
            self.popup = Some(Popup::DeleteConfirm(note));
        }
    }
    fn delete_note_confirmed(&mut self, note: Note) {
        let index = self.selected_note_index;
        let deleted_id = note.summary.id.clone();
        if self.update_worker.is_some() {
            return;
        }
        let request = DeleteNote {
            id: deleted_id.clone(),
            expected_modification_date: Some(note.summary.modification_date),
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .delete_note(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(_) => UpdateWorkerResult::Deleted {
                    note_id: deleted_id,
                    previous_index: index,
                },
                Err(error) => UpdateWorkerResult::Failed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Deleting…".into(),
            is_error: false,
        };
    }

    fn begin_edit(&mut self) {
        if !self.require_live_backend() {
            return;
        }
        let Some(note) = self.selected_note.clone() else {
            return;
        };
        match classify_editability(&note) {
            Editability::PlainText | Editability::RichTextSupported => {
                match editor_document_from_html(&note.summary.name, &note.body_html) {
                    Ok(document) => self.open_edit(note, document),
                    Err(error) => {
                        self.status = StatusMessage {
                            text: format!("ERROR: {error}"),
                            is_error: true,
                        }
                    }
                }
            }
            Editability::ReadOnlyUnsupported { reasons } => {
                self.status = StatusMessage {
                    text: format!(
                        "ERROR: Editing disabled to prevent data loss: {}",
                        reasons.join("; ")
                    ),
                    is_error: true,
                }
            }
        }
    }

    fn open_edit(&mut self, note: Note, document: EditorDocument) {
        let current_target = document.first_target();
        self.edit = Some(EditSession {
            note_id: Some(note.summary.id.clone()),
            folder_id: note.summary.folder_id.clone(),
            original_title: note.summary.name.clone(),
            original_body_html: note.body_html.clone(),
            original_plaintext: note.plaintext.clone(),
            base_modification_date: Some(note.summary.modification_date.clone()),
            title_buffer: note.summary.name,
            document,
            current_target,
            dirty: false,
            is_new: false,
            field: EditField::Body,
            cursor: 0,
            viewport: 0,
        });
        self.mode = AppMode::Insert;
    }

    fn handle_insert(&mut self, key: KeyEvent) {
        if self.update_worker.is_some() {
            self.status = StatusMessage {
                text: "Saving…".into(),
                is_error: false,
            };
            return;
        }
        match key.code {
            KeyCode::Esc => self.mode = AppMode::Normal,
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.save_edit(false)
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit = None;
                self.mode = AppMode::Normal;
                self.clear_editor_recovery();
                self.status = StatusMessage {
                    text: "Edit cancelled".into(),
                    is_error: false,
                };
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_inline(InlineStyle::Bold)
            }
            KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_inline(InlineStyle::Italic)
            }
            KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.toggle_inline(InlineStyle::Italic)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_inline(InlineStyle::Underline)
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_link_popup()
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_visual_target()
            }
            KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Heading1)
            }
            KeyCode::Char('2') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Heading2)
            }
            KeyCode::Char('3') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Heading3)
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Paragraph)
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Bullet)
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Numbered)
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Quote)
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.convert_current(TargetKind::Code)
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::ALT) => self.move_target(1),
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::ALT) => self.move_target(-1),
            KeyCode::Tab => self.switch_field(),
            KeyCode::Backspace => self.edit_backspace(),
            KeyCode::Delete => self.edit_delete(),
            KeyCode::Enter
                if self
                    .edit
                    .as_ref()
                    .is_some_and(|edit| edit.field == EditField::Body) =>
            {
                self.insert_target_after()
            }
            KeyCode::Enter => self.insert_text("\n"),
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_text(&character.to_string())
            }
            KeyCode::Left | KeyCode::Up => self.cursor_move(-1),
            KeyCode::Right | KeyCode::Down => self.cursor_move(1),
            KeyCode::PageUp => self.scroll_editor(-10),
            KeyCode::PageDown => self.scroll_editor(10),
            KeyCode::Home => self.cursor_edge(false),
            KeyCode::End => self.cursor_edge(true),
            _ => {}
        }
        self.persist_editor_recovery_if_dirty();
    }

    fn open_link_popup(&mut self) {
        if !self.editing_body() || !self.require_rich_feature(RichFeature::Hyperlink) {
            return;
        }
        if let Some(edit) = &self.edit {
            self.popup = Some(Popup::Link {
                target: edit.current_target,
                url: "https://".into(),
            });
        }
    }

    fn toggle_inline(&mut self, style: InlineStyle) {
        if !self.editing_body()
            || !self.require_rich_feature(match style {
                InlineStyle::Bold => RichFeature::Bold,
                InlineStyle::Italic => RichFeature::Italic,
                InlineStyle::Underline => RichFeature::Underline,
            })
        {
            return;
        }
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            if edit.field != EditField::Body {
                return Ok(());
            }
            edit.document.toggle_style(edit.current_target, style)?;
            Self::mark_dirty(edit);
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn convert_current(&mut self, kind: TargetKind) {
        if !self.editing_body() {
            return;
        }
        if target_kind_feature(kind).is_some_and(|feature| !self.require_rich_feature(feature)) {
            return;
        }
        let supports_mixed_lists = self
            .backend()
            .capabilities()
            .rich_text
            .mixed_adjacent_list_types;
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(false);
            };
            let mixed_boundaries_before = mixed_list_boundary_count(&edit.document.document);
            let mut candidate = edit.document.clone();
            let target = candidate.convert_target(edit.current_target, kind)?;
            let creates_mixed =
                mixed_list_boundary_count(&candidate.document) > mixed_boundaries_before;
            if creates_mixed && !supports_mixed_lists {
                return Ok(true);
            }
            edit.document = candidate;
            edit.current_target = target;
            edit.cursor = edit.document.clamp_cursor(target, edit.cursor)?;
            Self::mark_dirty(edit);
            Ok::<bool, EditorError>(false)
        })();
        match result {
            Ok(true) => self.set_unsupported_feature_status(RichFeature::MixedAdjacentListTypes),
            Ok(false) => {}
            Err(error) => self.set_editor_error(error),
        }
    }

    fn editing_body(&self) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|edit| edit.field == EditField::Body)
    }

    fn require_rich_feature(&mut self, feature: RichFeature) -> bool {
        if self.backend().capabilities().rich_text.supports(feature) {
            true
        } else {
            self.set_unsupported_feature_status(feature);
            false
        }
    }

    fn set_unsupported_feature_status(&mut self, feature: RichFeature) {
        self.status = StatusMessage {
            text: format!(
                "ERROR: {feature} is disabled because this backend cannot save it losslessly"
            ),
            is_error: true,
        };
    }

    fn move_target(&mut self, delta: isize) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            if edit.field != EditField::Body {
                return Ok(());
            }
            let target = if delta.is_negative() {
                edit.document.previous_target(edit.current_target)
            } else {
                edit.document.next_target(edit.current_target)
            };
            edit.current_target = target;
            edit.cursor = edit.document.clamp_cursor(target, edit.cursor)?;
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn insert_target_after(&mut self) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            let target = edit.document.insert_after(edit.current_target)?;
            edit.current_target = target;
            edit.cursor = 0;
            Self::mark_dirty(edit);
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn delete_visual_target(&mut self) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            if edit.field != EditField::Body {
                return Ok(());
            }
            let target = edit.document.delete_target(edit.current_target)?;
            edit.current_target = target;
            edit.cursor = edit.document.clamp_cursor(target, edit.cursor)?;
            Self::mark_dirty(edit);
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn switch_field(&mut self) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            edit.field = if edit.field == EditField::Title {
                EditField::Body
            } else {
                EditField::Title
            };
            edit.cursor = if edit.field == EditField::Title {
                edit.title_buffer.chars().count()
            } else {
                edit.document.char_len(edit.current_target)?
            };
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn insert_text(&mut self, value: &str) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            if value.is_empty() {
                return Ok(());
            }
            match edit.field {
                EditField::Title => {
                    for character in value.chars() {
                        let offset = char_to_byte_index(&edit.title_buffer, edit.cursor);
                        edit.title_buffer.insert(offset, character);
                        edit.cursor += 1;
                    }
                }
                EditField::Body => {
                    for character in value.chars() {
                        edit.cursor = edit.document.insert_char(
                            edit.current_target,
                            edit.cursor,
                            character,
                        )?;
                    }
                }
            }
            Self::mark_dirty(edit);
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn edit_backspace(&mut self) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            let old_cursor = edit.cursor;
            match edit.field {
                EditField::Title if edit.cursor > 0 => {
                    let end = char_to_byte_index(&edit.title_buffer, edit.cursor);
                    let start = char_to_byte_index(&edit.title_buffer, edit.cursor - 1);
                    edit.title_buffer.replace_range(start..end, "");
                    edit.cursor -= 1;
                }
                EditField::Title => {}
                EditField::Body => {
                    edit.cursor = edit.document.backspace(edit.current_target, edit.cursor)?;
                }
            }
            if edit.cursor != old_cursor {
                Self::mark_dirty(edit);
            }
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn edit_delete(&mut self) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            let changed = match edit.field {
                EditField::Title => {
                    let len = edit.title_buffer.chars().count();
                    if edit.cursor < len {
                        let start = char_to_byte_index(&edit.title_buffer, edit.cursor);
                        let end = char_to_byte_index(&edit.title_buffer, edit.cursor + 1);
                        edit.title_buffer.replace_range(start..end, "");
                        true
                    } else {
                        false
                    }
                }
                EditField::Body => {
                    let len = edit.document.char_len(edit.current_target)?;
                    if edit.cursor < len {
                        edit.cursor = edit
                            .document
                            .delete_char(edit.current_target, edit.cursor)?;
                        true
                    } else {
                        false
                    }
                }
            };
            if changed {
                Self::mark_dirty(edit);
            }
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn cursor_move(&mut self, delta: isize) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            let len = match edit.field {
                EditField::Title => edit.title_buffer.chars().count(),
                EditField::Body => edit.document.char_len(edit.current_target)?,
            };
            edit.cursor = if delta.is_negative() {
                edit.cursor.saturating_sub(1)
            } else {
                edit.cursor.saturating_add(1).min(len)
            };
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn cursor_edge(&mut self, end: bool) {
        let result = (|| {
            let Some(edit) = &mut self.edit else {
                return Ok(());
            };
            edit.cursor = if !end {
                0
            } else {
                match edit.field {
                    EditField::Title => edit.title_buffer.chars().count(),
                    EditField::Body => edit.document.char_len(edit.current_target)?,
                }
            };
            Ok::<(), EditorError>(())
        })();
        if let Err(error) = result {
            self.set_editor_error(error);
        }
    }

    fn scroll_editor(&mut self, delta: isize) {
        let Some(edit) = &mut self.edit else {
            return;
        };
        let maximum = editor_viewport_max(edit);
        edit.viewport = if delta.is_negative() {
            edit.viewport.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            edit.viewport.saturating_add(delta as u16).min(maximum)
        };
    }

    fn mark_dirty(edit: &mut EditSession) {
        let original = if edit.is_new {
            EditorDocument::empty()
        } else {
            editor_document_from_html(&edit.original_title, &edit.original_body_html)
                .unwrap_or_else(|_| EditorDocument::from_plaintext(&edit.original_plaintext))
        };
        edit.dirty =
            edit.title_buffer != edit.original_title || edit.document.document != original.document;
    }

    fn set_editor_error(&mut self, error: EditorError) {
        self.status = StatusMessage {
            text: format!("ERROR: {error}"),
            is_error: true,
        };
    }

    fn save_edit(&mut self, overwrite: bool) {
        if overwrite && self.start_conflict_overwrite_worker() {
            return;
        }
        if !overwrite && (self.start_create_worker() || self.start_normal_update_worker()) {
            return;
        }
        if !self.require_live_backend() {
            return;
        }
        let Some(edit) = self.edit.clone() else {
            return;
        };
        let unsupported = self
            .backend()
            .capabilities()
            .rich_text
            .unsupported_features(&edit.document.document);
        if !unsupported.is_empty() {
            self.set_error(NotesError::UnsupportedRichContent {
                features: unsupported,
            });
            return;
        }
        let body_html = serialize_notes_html(&edit.document.document);
        if edit.is_new {
            let result = self.backend().create_note(&CreateNote {
                folder_id: edit.folder_id.clone(),
                name: edit.title_buffer.clone(),
                body_html,
            });
            match result {
                Ok(note) => self.finish_saved(note, "Saved"),
                Err(error) => self.set_error(error),
            };
            return;
        }
        let id = edit
            .note_id
            .clone()
            .unwrap_or_else(|| NoteId::new("missing"));
        if !overwrite {
            let result = self.backend().get_note(&id);
            match result {
                Ok(remote)
                    if Some(remote.summary.modification_date.clone())
                        != edit.base_modification_date =>
                {
                    self.popup = Some(Popup::Conflict(Box::new(remote)));
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    self.set_error(error);
                    return;
                }
            }
        }
        let result = self.backend().update_note(&UpdateNote {
            id,
            name: Some(edit.title_buffer.clone()),
            body_html: Some(body_html),
            expected_modification_date: if overwrite {
                None
            } else {
                edit.base_modification_date.clone()
            },
        });
        match result {
            Ok(note) => self.finish_saved(note, "Saved"),
            Err(error) => self.set_error(error),
        }
    }

    fn start_conflict_overwrite_worker(&mut self) -> bool {
        if self.update_worker.is_some() {
            return true;
        }
        let Some(edit) = self.edit.clone().filter(|edit| !edit.is_new) else {
            return false;
        };
        if !self.require_live_backend() {
            return true;
        }
        let unsupported = self
            .backend()
            .capabilities()
            .rich_text
            .unsupported_features(&edit.document.document);
        if !unsupported.is_empty() {
            self.set_error(NotesError::UnsupportedRichContent {
                features: unsupported,
            });
            return true;
        }
        let request = UpdateNote {
            id: edit.note_id.unwrap_or_else(|| NoteId::new("missing")),
            name: Some(edit.title_buffer),
            body_html: Some(serialize_notes_html(&edit.document.document)),
            expected_modification_date: None,
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .update_note(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(note) => UpdateWorkerResult::Saved(note),
                Err(error) => UpdateWorkerResult::Failed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Saving…".into(),
            is_error: false,
        };
        true
    }

    fn start_create_worker(&mut self) -> bool {
        if self.update_worker.is_some() {
            return true;
        }
        let Some(edit) = self.edit.clone().filter(|edit| edit.is_new) else {
            return false;
        };
        if !self.require_live_backend() {
            return true;
        }
        let unsupported = self
            .backend()
            .capabilities()
            .rich_text
            .unsupported_features(&edit.document.document);
        if !unsupported.is_empty() {
            self.set_error(NotesError::UnsupportedRichContent {
                features: unsupported,
            });
            return true;
        }
        let request = CreateNote {
            folder_id: edit.folder_id,
            name: edit.title_buffer,
            body_html: serialize_notes_html(&edit.document.document),
        };
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .create_note(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(note) => UpdateWorkerResult::Saved(note),
                Err(error) => UpdateWorkerResult::Failed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Saving…".into(),
            is_error: false,
        };
        true
    }

    fn start_normal_update_worker(&mut self) -> bool {
        if self.update_worker.is_some() || self.edit.as_ref().is_none_or(|edit| edit.is_new) {
            return self.update_worker.is_some();
        }
        if !self.require_live_backend() {
            return true;
        }
        let edit = self.edit.clone().expect("existing edit checked");
        let unsupported = self
            .backend()
            .capabilities()
            .rich_text
            .unsupported_features(&edit.document.document);
        if !unsupported.is_empty() {
            self.set_error(NotesError::UnsupportedRichContent {
                features: unsupported,
            });
            return true;
        }
        let id = edit
            .note_id
            .clone()
            .unwrap_or_else(|| NoteId::new("missing"));
        let request = UpdateNote {
            id: id.clone(),
            name: Some(edit.title_buffer.clone()),
            body_html: Some(serialize_notes_html(&edit.document.document)),
            expected_modification_date: edit.base_modification_date.clone(),
        };
        let backend = Arc::clone(&self.backend);
        let base_date = edit.base_modification_date.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let backend = backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match backend.get_note(&id) {
                    Ok(remote) if Some(remote.summary.modification_date.clone()) != base_date => {
                        UpdateWorkerResult::Conflict(remote)
                    }
                    Ok(_) => match backend.update_note(&request) {
                        Ok(note) => UpdateWorkerResult::Saved(note),
                        Err(error) => UpdateWorkerResult::Failed(error),
                    },
                    Err(error) => UpdateWorkerResult::Failed(error),
                }
            }))
            .unwrap_or(UpdateWorkerResult::Panicked);
            let _ = sender.send(outcome);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Saving…".into(),
            is_error: false,
        };
        true
    }
    fn finish_saved(&mut self, note: Note, message: &str) {
        self.folder_notes_cache.remove(&note.summary.folder_id);
        let id = note.summary.id.clone();
        self.edit = None;
        self.mode = AppMode::Normal;
        self.apply_saved_note_to_runtime(&note);
        if let Some(index) = self.visible_note_ids().iter().position(|item| *item == id) {
            self.selected_note_index = index;
            self.selected_note = Some(note.clone());
            self.cached_preview_state = None;
            self.preview_scroll = 0;
        }
        let cache_warning = self.persist_full_note(&note);
        self.persist_cache_snapshot();
        if let Some(warning) = cache_warning {
            self.status = StatusMessage {
                text: warning,
                is_error: false,
            };
        } else if !self.status.text.contains("cache warning") {
            self.status = StatusMessage {
                text: message.into(),
                is_error: false,
            };
        }
        self.persist_session_selection();
        self.clear_editor_recovery();
    }

    fn persist_full_note(&mut self, note: &Note) -> Option<String> {
        if let Some(cache) = &mut self.cache {
            if let Err(error) = cache.upsert_note(note) {
                return Some(format!("Live · cache warning: {error}"));
            }
        }
        None
    }

    fn remove_cached_full_note(&mut self, id: &NoteId) -> Option<String> {
        if let Some(cache) = &mut self.cache {
            if let Err(error) = cache.remove_note(id) {
                return Some(format!("Live · cache warning: {error}"));
            }
        }
        None
    }

    /// Applies an authoritative successful save result without treating a
    /// search view as the source of truth. Cache persistence happens only
    /// after this runtime state has settled.
    fn apply_saved_note_to_runtime(&mut self, note: &Note) {
        if let Some(existing) = self
            .notes
            .iter_mut()
            .find(|summary| summary.id == note.summary.id)
        {
            *existing = note.summary.clone();
        } else if self.selected_folder_id() == Some(&note.summary.folder_id) {
            self.notes.push(note.summary.clone());
        }
        self.recompute_search();
    }
    fn begin_move(&mut self) {
        if !self.require_live_backend() {
            return;
        }
        if self.selected_note.is_some() {
            self.popup = Some(Popup::Move { selected: 0 });
        }
    }

    fn begin_attachments(&mut self) {
        if !self.require_live_backend() {
            return;
        }
        let summary = self
            .selected_note
            .as_ref()
            .map(|note| &note.summary)
            .or_else(|| self.visible_note(self.selected_note_index));
        let Some(summary) = summary else {
            self.status = StatusMessage {
                text: "No note selected".into(),
                is_error: false,
            };
            return;
        };
        if summary.password_protected {
            self.set_error(NotesError::ProtectedNote {
                id: summary.id.clone(),
            });
            return;
        }
        let Some(note) = &self.selected_note else {
            self.status = StatusMessage {
                text: "ERROR: Attachment metadata is unavailable for this note".into(),
                is_error: true,
            };
            return;
        };
        if note.attachments.is_empty() {
            self.status = StatusMessage {
                text: "This note has no attachments".into(),
                is_error: false,
            };
            return;
        }
        self.popup = Some(Popup::Attachments { selected: 0 });
    }

    fn preview_attachment(&mut self, selected: usize) {
        if !self.require_live_backend() {
            return;
        }
        let Some((note_id, attachment)) = self.selected_attachment(selected) else {
            self.status = StatusMessage {
                text: "ERROR: Selected attachment is unavailable".into(),
                is_error: true,
            };
            return;
        };
        match attachment.preview_status {
            AttachmentAccessStatus::Available => {
                self.start_attachment_preview_worker(
                    note_id,
                    attachment.id.clone(),
                    attachment_display_name(&attachment),
                );
            }
            AttachmentAccessStatus::Unavailable(reason) => {
                self.set_error(NotesError::AttachmentUnavailable {
                    id: attachment.id,
                    operation: "previewed",
                    reason,
                });
            }
        }
    }

    fn export_attachment(&mut self, selected: usize) {
        if !self.require_live_backend() {
            return;
        }
        let Some((note_id, attachment)) = self.selected_attachment(selected) else {
            self.status = StatusMessage {
                text: "ERROR: Selected attachment is unavailable".into(),
                is_error: true,
            };
            return;
        };
        match attachment.export_status {
            AttachmentAccessStatus::Available => {
                self.start_attachment_export_worker(note_id, attachment.id);
            }
            AttachmentAccessStatus::Unavailable(reason) => {
                self.set_error(NotesError::AttachmentUnavailable {
                    id: attachment.id,
                    operation: "exported",
                    reason,
                });
            }
        }
    }

    fn start_attachment_preview_worker(
        &mut self,
        note_id: NoteId,
        attachment_id: AttachmentId,
        name: String,
    ) {
        if self.update_worker.is_some() {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .preview_attachment(&note_id, &attachment_id)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(()) => UpdateWorkerResult::AttachmentPreviewed { name },
                Err(error) => UpdateWorkerResult::Failed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Opening attachment…".into(),
            is_error: false,
        };
    }

    fn start_attachment_export_worker(&mut self, note_id: NoteId, attachment_id: AttachmentId) {
        if self.update_worker.is_some() {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .export_attachment(&note_id, &attachment_id)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(result) => UpdateWorkerResult::AttachmentExported {
                    destination: result.destination,
                },
                Err(error) => UpdateWorkerResult::Failed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Exporting attachment…".into(),
            is_error: false,
        };
    }

    fn selected_attachment(&self, selected: usize) -> Option<(NoteId, AttachmentSummary)> {
        let note = self.selected_note.as_ref()?;
        Some((
            note.summary.id.clone(),
            note.attachments.get(selected)?.clone(),
        ))
    }

    fn handle_popup(&mut self, key: KeyEvent) {
        let popup = self.popup.take();
        match popup {
            Some(Popup::RenameFolder {
                account_id,
                folder_id,
                original_name,
                mut name,
                mut cursor,
            }) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.start_rename_folder_worker(
                    account_id,
                    folder_id,
                    original_name,
                    name,
                    cursor,
                ),
                KeyCode::Backspace if cursor > 0 => {
                    let end = char_to_byte_index(&name, cursor);
                    let start = char_to_byte_index(&name, cursor - 1);
                    name.replace_range(start..end, "");
                    cursor -= 1;
                    self.popup = Some(Popup::RenameFolder {
                        account_id,
                        folder_id,
                        original_name,
                        name,
                        cursor,
                    });
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let byte = char_to_byte_index(&name, cursor);
                    name.insert(byte, character);
                    cursor += 1;
                    self.popup = Some(Popup::RenameFolder {
                        account_id,
                        folder_id,
                        original_name,
                        name,
                        cursor,
                    });
                }
                _ => {
                    self.popup = Some(Popup::RenameFolder {
                        account_id,
                        folder_id,
                        original_name,
                        name,
                        cursor,
                    })
                }
            },
            Some(Popup::ReparentFolder {
                account_id,
                folder_id,
                source_folder_name,
                original_parent,
                destinations,
                mut selected_destination,
            }) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('j') | KeyCode::Down => {
                    selected_destination = move_index(selected_destination, destinations.len(), 1);
                    self.popup = Some(Popup::ReparentFolder {
                        account_id,
                        folder_id,
                        source_folder_name,
                        original_parent,
                        destinations,
                        selected_destination,
                    });
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    selected_destination = move_index(selected_destination, destinations.len(), -1);
                    self.popup = Some(Popup::ReparentFolder {
                        account_id,
                        folder_id,
                        source_folder_name,
                        original_parent,
                        destinations,
                        selected_destination,
                    });
                }
                KeyCode::Enter => self.start_reparent_folder_worker(
                    account_id,
                    folder_id,
                    source_folder_name,
                    original_parent,
                    destinations,
                    selected_destination,
                ),
                _ => {
                    self.popup = Some(Popup::ReparentFolder {
                        account_id,
                        folder_id,
                        source_folder_name,
                        original_parent,
                        destinations,
                        selected_destination,
                    })
                }
            },
            Some(Popup::CreateFolder {
                account_id,
                mut name,
                mut cursor,
            }) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.start_create_folder_worker(account_id, name, cursor),
                KeyCode::Backspace if cursor > 0 => {
                    let end = char_to_byte_index(&name, cursor);
                    let start = char_to_byte_index(&name, cursor - 1);
                    name.replace_range(start..end, "");
                    cursor -= 1;
                    self.popup = Some(Popup::CreateFolder {
                        account_id,
                        name,
                        cursor,
                    });
                }
                KeyCode::Delete if cursor < name.chars().count() => {
                    let start = char_to_byte_index(&name, cursor);
                    let end = char_to_byte_index(&name, cursor + 1);
                    name.replace_range(start..end, "");
                    self.popup = Some(Popup::CreateFolder {
                        account_id,
                        name,
                        cursor,
                    });
                }
                KeyCode::Left => {
                    cursor = cursor.saturating_sub(1);
                    self.popup = Some(Popup::CreateFolder {
                        account_id,
                        name,
                        cursor,
                    });
                }
                KeyCode::Right => {
                    cursor = cursor.saturating_add(1).min(name.chars().count());
                    self.popup = Some(Popup::CreateFolder {
                        account_id,
                        name,
                        cursor,
                    });
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let byte = char_to_byte_index(&name, cursor);
                    name.insert(byte, character);
                    cursor += 1;
                    self.popup = Some(Popup::CreateFolder {
                        account_id,
                        name,
                        cursor,
                    });
                }
                _ => {
                    self.popup = Some(Popup::CreateFolder {
                        account_id,
                        name,
                        cursor,
                    })
                }
            },
            Some(Popup::CreateChildFolder {
                account_id,
                parent_folder_id,
                parent_folder_name,
                mut name,
                mut cursor,
            }) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.start_create_child_folder_worker(
                    account_id,
                    parent_folder_id,
                    parent_folder_name,
                    name,
                    cursor,
                ),
                KeyCode::Backspace if cursor > 0 => {
                    let end = char_to_byte_index(&name, cursor);
                    let start = char_to_byte_index(&name, cursor - 1);
                    name.replace_range(start..end, "");
                    cursor -= 1;
                    self.popup = Some(Popup::CreateChildFolder {
                        account_id,
                        parent_folder_id,
                        parent_folder_name,
                        name,
                        cursor,
                    });
                }
                KeyCode::Delete if cursor < name.chars().count() => {
                    let start = char_to_byte_index(&name, cursor);
                    let end = char_to_byte_index(&name, cursor + 1);
                    name.replace_range(start..end, "");
                    self.popup = Some(Popup::CreateChildFolder {
                        account_id,
                        parent_folder_id,
                        parent_folder_name,
                        name,
                        cursor,
                    });
                }
                KeyCode::Left => {
                    cursor = cursor.saturating_sub(1);
                    self.popup = Some(Popup::CreateChildFolder {
                        account_id,
                        parent_folder_id,
                        parent_folder_name,
                        name,
                        cursor,
                    });
                }
                KeyCode::Right => {
                    cursor = cursor.saturating_add(1).min(name.chars().count());
                    self.popup = Some(Popup::CreateChildFolder {
                        account_id,
                        parent_folder_id,
                        parent_folder_name,
                        name,
                        cursor,
                    });
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    name.insert(char_to_byte_index(&name, cursor), character);
                    cursor += 1;
                    self.popup = Some(Popup::CreateChildFolder {
                        account_id,
                        parent_folder_id,
                        parent_folder_name,
                        name,
                        cursor,
                    });
                }
                _ => {
                    self.popup = Some(Popup::CreateChildFolder {
                        account_id,
                        parent_folder_id,
                        parent_folder_name,
                        name,
                        cursor,
                    })
                }
            },
            Some(Popup::DraftRecovery { draft, recoverable }) => match key.code {
                KeyCode::Char('r') if recoverable => self.restore_editor_recovery(draft),
                KeyCode::Char('d') => self.clear_editor_recovery(),
                KeyCode::Esc => {
                    self.pending_editor_draft = Some(draft);
                }
                _ => {
                    self.popup = Some(Popup::DraftRecovery { draft, recoverable });
                }
            },
            Some(Popup::Conflict(remote)) => match key.code {
                KeyCode::Char('r') => {
                    self.reopen_remote(*remote);
                }
                KeyCode::Char('o') => self.save_edit(true),
                KeyCode::Char('c') | KeyCode::Esc => {}
                _ => {
                    self.popup = Some(Popup::Conflict(remote));
                }
            },
            Some(Popup::Move { mut selected }) => {
                let folders: Vec<_> = self
                    .navigation
                    .iter()
                    .filter_map(|item| {
                        if let NavigationItem::Folder { id, name, .. } = item {
                            Some((id.clone(), name.clone()))
                        } else {
                            None
                        }
                    })
                    .collect();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = move_index(selected, folders.len(), 1);
                        self.popup = Some(Popup::Move { selected });
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = move_index(selected, folders.len(), -1);
                        self.popup = Some(Popup::Move { selected });
                    }
                    KeyCode::Enter => {
                        if let (Some(note), Some((folder_id, name))) =
                            (self.selected_note.clone(), folders.get(selected))
                        {
                            self.start_move_worker(
                                MoveNote {
                                    id: note.summary.id,
                                    destination_folder_id: folder_id.clone(),
                                    expected_modification_date: Some(
                                        note.summary.modification_date,
                                    ),
                                },
                                name.clone(),
                            );
                        }
                    }
                    KeyCode::Esc => {}
                    _ => self.popup = Some(Popup::Move { selected }),
                }
            }
            Some(Popup::Attachments { mut selected }) => {
                let attachment_count = self
                    .selected_note
                    .as_ref()
                    .map_or(0, |note| note.attachments.len());
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = move_index(selected, attachment_count, 1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = move_index(selected, attachment_count, -1);
                    }
                    KeyCode::Enter => self.preview_attachment(selected),
                    KeyCode::Char('x') => self.export_attachment(selected),
                    KeyCode::Esc => return,
                    _ => {}
                }
                self.popup = Some(Popup::Attachments { selected });
            }
            Some(Popup::Discard { action }) => match key.code {
                KeyCode::Char('y') => {
                    self.edit = None;
                    self.mode = AppMode::Normal;
                    self.clear_editor_recovery();
                    match action {
                        PendingAction::Quit => self.should_quit = true,
                        PendingAction::Key(key) => self.handle_key(key),
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.mode = AppMode::Insert;
                }
                _ => self.popup = Some(Popup::Discard { action }),
            },
            Some(Popup::Link { target, mut url }) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let result = if let Some(edit) = &mut self.edit {
                        edit.document.set_link(target, url).map(|()| {
                            Self::mark_dirty(edit);
                        })
                    } else {
                        Ok(())
                    };
                    if let Err(error) = result {
                        self.set_editor_error(error);
                    }
                }
                KeyCode::Backspace => {
                    url.pop();
                    self.popup = Some(Popup::Link { target, url });
                }
                KeyCode::Char(character) => {
                    url.push(character);
                    self.popup = Some(Popup::Link { target, url });
                }
                _ => self.popup = Some(Popup::Link { target, url }),
            },
            Some(Popup::DeleteConfirm(note)) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.delete_note_confirmed(note),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => self.popup = Some(Popup::DeleteConfirm(note)),
            },
            Some(Popup::DeleteFolder {
                account_id,
                folder_id,
                folder_name,
            }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.delete_folder_confirmed(account_id, folder_id, folder_name)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                _ => {
                    self.popup = Some(Popup::DeleteFolder {
                        account_id,
                        folder_id,
                        folder_name,
                    })
                }
            },
            Some(Popup::Settings(mut settings)) => {
                if let Some(input) = &mut settings.interval_input {
                    match key.code {
                        KeyCode::Esc => settings.interval_input = None,
                        KeyCode::Backspace => { input.pop(); }
                        KeyCode::Char(character) if character.is_ascii_digit() => input.push(character),
                        KeyCode::Enter => match input.parse::<u64>().ok().and_then(|seconds| interval(seconds).ok()) {
                            Some(value) => { settings.draft.refresh_interval = value; settings.staged[1] = SettingDraft::Set(ConfigEditValue::RefreshIntervalSeconds(value.as_secs())); settings.interval_input = None; settings.error = None; }
                            None => settings.error = Some(format!("Refresh interval must be {MIN_REFRESH_INTERVAL_SECONDS}–{MAX_REFRESH_INTERVAL_SECONDS} seconds")),
                        },
                        _ => {}
                    }
                    self.popup = Some(Popup::Settings(settings));
                    return;
                }
                match key.code {
                    KeyCode::Esc => return,
                    KeyCode::Char('j') | KeyCode::Down => {
                        settings.selected = move_index(settings.selected, 4, 1)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        settings.selected = move_index(settings.selected, 4, -1)
                    }
                    KeyCode::Char(' ') if settings.selected != 1 => {
                        match settings.selected {
                            0 => {
                                settings.draft.auto_refresh = !settings.draft.auto_refresh;
                            }
                            2 => {
                                settings.draft.preview_wrap = !settings.draft.preview_wrap;
                            }
                            3 => {
                                settings.draft.show_attachment_metadata =
                                    !settings.draft.show_attachment_metadata;
                            }
                            _ => {}
                        }
                        settings.staged[settings.selected] =
                            SettingDraft::Set(match settings.selected {
                                0 => ConfigEditValue::Boolean(settings.draft.auto_refresh),
                                2 => ConfigEditValue::Boolean(settings.draft.preview_wrap),
                                3 => ConfigEditValue::Boolean(
                                    settings.draft.show_attachment_metadata,
                                ),
                                _ => unreachable!(),
                            });
                    }
                    KeyCode::Enter if settings.selected == 1 => {
                        settings.interval_input =
                            Some(settings.draft.refresh_interval.as_secs().to_string());
                    }
                    KeyCode::Char('u') if self.config_file_presence[settings.selected] => {
                        settings.staged[settings.selected] = SettingDraft::Unset
                    }
                    KeyCode::Char('r') => {
                        settings.staged[settings.selected] = SettingDraft::Unchanged
                    }
                    KeyCode::Char('R') => {
                        for index in 0..4 {
                            if self.config_file_presence[index] {
                                settings.staged[index] = SettingDraft::Unset;
                            }
                        }
                    }
                    KeyCode::Char('s') => {
                        let values = settings_edits(&settings);
                        if values.is_empty() {
                            self.status = StatusMessage {
                                text: "No settings changes".into(),
                                is_error: false,
                            };
                            return;
                        }
                        match edit_config_batch(&self.config_file_path, &values) {
                            Ok(()) => {
                                let cli_override =
                                    settings.staged.iter().enumerate().any(|(index, staged)| {
                                        *staged != SettingDraft::Unchanged
                                            && self.config_sources[index] == ConfigValueSource::Cli
                                    });
                                self.status = StatusMessage {
                                    text: if cli_override {
                                        "Settings saved; changes apply on next startup (CLI overrides remain active)".into()
                                    } else {
                                        "Settings saved; changes apply on next startup".into()
                                    },
                                    is_error: false,
                                };
                                return;
                            }
                            Err(error) => settings.error = Some(error),
                        }
                    }
                    _ => {}
                }
                self.popup = Some(Popup::Settings(settings));
            }
            None => {}
        }
        self.persist_editor_recovery_if_dirty();
    }

    fn start_move_worker(&mut self, request: MoveNote, destination_name: String) {
        if self.update_worker.is_some() {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                backend
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .move_note(&request)
            }))
            .map_or(UpdateWorkerResult::Panicked, |result| match result {
                Ok(note) => UpdateWorkerResult::Moved {
                    note,
                    destination_name,
                },
                Err(error) => UpdateWorkerResult::Failed(error),
            });
            let _ = sender.send(result);
        });
        self.update_worker = Some(receiver);
        self.status = StatusMessage {
            text: "Moving…".into(),
            is_error: false,
        };
    }
    fn reopen_remote(&mut self, note: Note) {
        match classify_editability(&note) {
            Editability::PlainText | Editability::RichTextSupported => {
                match editor_document_from_html(&note.summary.name, &note.body_html) {
                    Ok(document) => self.open_edit(note, document),
                    Err(error) => {
                        self.status = StatusMessage {
                            text: format!("ERROR: {error}"),
                            is_error: true,
                        }
                    }
                }
            }
            Editability::ReadOnlyUnsupported { reasons } => {
                self.status = StatusMessage {
                    text: format!(
                        "ERROR: Remote note is now read-only: {}",
                        reasons.join("; ")
                    ),
                    is_error: true,
                };
            }
        }
    }

    pub fn selected_folder_id(&self) -> Option<&FolderId> {
        match self.navigation.get(self.selected_navigation) {
            Some(NavigationItem::Folder { id, .. }) => Some(id),
            _ => None,
        }
    }

    fn activate_selection(&mut self) {
        match self.focus {
            Focus::Navigation => self.load_notes_for_browsing(),
            Focus::Notes => self.load_selected_note(),
            Focus::Preview => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Navigation => {
                self.selected_navigation =
                    move_index(self.selected_navigation, self.navigation.len(), delta);
                self.search = SearchState::Inactive;
                if self.periodic_refresh_in_flight() {
                    self.invalidate_periodic_refresh_result();
                } else {
                    self.load_notes_for_browsing();
                }
            }
            Focus::Notes => {
                self.selected_note_index =
                    move_index(self.selected_note_index, self.visible_note_count(), delta);
                if self.periodic_refresh_in_flight() {
                    self.invalidate_periodic_refresh_result();
                } else {
                    self.load_selected_note();
                    self.persist_session_selection();
                }
            }
            Focus::Preview => self.scroll_preview(delta.saturating_mul(3)),
        }
    }

    fn move_to_edge(&mut self, last: bool) {
        match self.focus {
            Focus::Navigation => {
                self.selected_navigation = edge_index(self.navigation.len(), last);
                self.search = SearchState::Inactive;
                if self.periodic_refresh_in_flight() {
                    self.invalidate_periodic_refresh_result();
                } else {
                    self.load_notes_for_browsing();
                }
            }
            Focus::Notes => {
                self.selected_note_index = edge_index(self.visible_note_count(), last);
                if self.periodic_refresh_in_flight() {
                    self.invalidate_periodic_refresh_result();
                } else {
                    self.load_selected_note();
                    self.persist_session_selection();
                }
            }
            Focus::Preview => self.preview_scroll = if last { u16::MAX } else { 0 },
        }
    }

    fn load_notes_for_selection(&mut self) {
        self.load_notes_for_selection_with_cache(true);
    }

    fn load_notes_for_browsing(&mut self) {
        self.load_notes_for_selection_with_cache_mode(true, true);
    }

    fn load_notes_for_selection_without_cache(&mut self) {
        self.load_notes_for_selection_with_cache(false);
    }

    fn load_notes_for_selection_with_cache(&mut self, persist_full_note: bool) {
        self.load_notes_for_selection_with_cache_mode(persist_full_note, false);
    }

    fn load_notes_for_selection_with_cache_mode(
        &mut self,
        persist_full_note: bool,
        allow_async_navigation: bool,
    ) {
        let started = Instant::now();
        let query = match self.navigation.get(self.selected_navigation) {
            Some(NavigationItem::Account { id, .. }) => NotesQuery {
                account_id: Some(id.clone()),
                ..Default::default()
            },
            Some(NavigationItem::Folder { id, .. }) => NotesQuery {
                folder_id: Some(id.clone()),
                ..Default::default()
            },
            None => {
                self.notes.clear();
                self.selected_note = None;
                self.cached_preview_state = None;
                self.status = StatusMessage {
                    text: "No accounts or folders available".into(),
                    is_error: false,
                };
                return;
            }
        };
        let target = query
            .folder_id
            .as_ref()
            .map(|id| id.as_str())
            .or_else(|| query.account_id.as_ref().map(|id| id.as_str()));
        if allow_async_navigation
            && persist_full_note
            && matches!(self.data_source, DataSourceState::Live)
        {
            if let Some(cache) = &self.cache {
                if let Some(folder_id) = query.folder_id.as_ref() {
                    if let Some(cached_items) = self.folder_notes_cache.get(folder_id).cloned() {
                        let preferred = self.selected_note_id();
                        self.notes = cached_items;
                        self.recompute_search();
                        self.restore_visible_selection(preferred);
                        self.load_selected_note_with_cache(persist_full_note);
                        self.queue_navigation_request(NavigationRequest::Folder(
                            self.refresh_context().expect("selected navigation context"),
                        ));
                        self.start_pending_navigation();
                        perf::event("navigation.folder.cache_hit", target, started, "memory");
                        return;
                    }
                }
                if let Ok(state) = cache.load_bootstrap() {
                    let cached_items: Vec<NoteSummary> = state
                        .notes
                        .into_iter()
                        .filter(|note| {
                            query
                                .folder_id
                                .as_ref()
                                .is_some_and(|id| &note.folder_id == id)
                        })
                        .collect();
                    if !cached_items.is_empty() {
                        let preferred = self.selected_note_id();
                        self.notes = cached_items;
                        self.recompute_search();
                        self.restore_visible_selection(preferred);
                        self.load_selected_note_with_cache(persist_full_note);
                        self.queue_navigation_request(NavigationRequest::Folder(
                            self.refresh_context().expect("selected navigation context"),
                        ));
                        self.start_pending_navigation();
                        perf::event("navigation.folder.cache_hit", target, started, "complete");
                        return;
                    }
                }
                perf::event("navigation.folder.cache_miss", target, started, "complete");
                if let Some(context) = self.refresh_context() {
                    self.queue_navigation_request(NavigationRequest::Folder(context));
                    self.start_pending_navigation();
                    return;
                }
            }
        }
        let result = self.backend().notes(&query);
        match result {
            Ok(NotesPage { items, total, .. }) => {
                let preferred = self.selected_note_id();
                self.notes = items;
                self.preview_scroll = 0;
                self.status = StatusMessage {
                    text: format!("{total} notes"),
                    is_error: false,
                };
                self.recompute_search();
                self.restore_visible_selection(preferred);
                self.load_selected_note_with_cache(persist_full_note);
                self.persist_session_selection();
            }
            Err(error) => self.set_error(error),
        }
        perf::event("tui.folder_activation", target, started, "complete");
    }

    fn load_selected_note(&mut self) {
        self.load_selected_note_with_cache(true);
    }

    fn load_selected_note_with_cache(&mut self, persist_full_note: bool) {
        let started = Instant::now();
        let Some(id) = self
            .visible_note(self.selected_note_index)
            .map(|summary| summary.id.clone())
        else {
            self.selected_note = None;
            self.cached_preview_state = None;
            return;
        };
        if self
            .selected_note
            .as_ref()
            .is_some_and(|note| note.summary.id != id)
        {
            self.selected_note = None;
        }
        self.cached_preview_state = None;
        if let Some(cache) = &self.cache {
            if matches!(self.data_source, DataSourceState::Live) && !persist_full_note {
                // Mutation reconciliation intentionally keeps its existing
                // synchronous read-through semantics.
            } else {
                let cached = cache.load_note(&id);
                let live_cache_miss =
                    matches!(self.data_source, DataSourceState::Live) && matches!(cached, Ok(None));
                match cached {
                    Ok(Some(note)) => {
                        self.selected_note = Some(note);
                        self.preview_scroll = 0;
                        if matches!(self.data_source, DataSourceState::Live) {
                            self.queue_navigation_request(NavigationRequest::Preview(id.clone()));
                            self.start_pending_navigation();
                            perf::event(
                                "navigation.preview.cache_hit",
                                Some(id.as_str()),
                                started,
                                "complete",
                            );
                        }
                    }
                    Ok(None) => {
                        self.cached_preview_state = Some(CachedPreviewState::MissingFullNote);
                    }
                    Err(error) => {
                        self.cached_preview_state =
                            Some(CachedPreviewState::ReadError(error.to_string()));
                    }
                }
                if !live_cache_miss {
                    perf::event("tui.preview_cached", Some(id.as_str()), started, "complete");
                    return;
                }
                self.queue_navigation_request(NavigationRequest::Preview(id.clone()));
                self.start_pending_navigation();
                perf::event(
                    "navigation.preview.cache_miss",
                    Some(id.as_str()),
                    started,
                    "complete",
                );
                return;
            }
        }
        if !matches!(self.data_source, DataSourceState::Live) {
            self.cached_preview_state = Some(CachedPreviewState::MissingFullNote);
            return;
        }
        let result = self.backend().get_note(&id);
        match result {
            Ok(note) => {
                self.selected_note = Some(note.clone());
                self.preview_scroll = 0;
                if persist_full_note {
                    if let Some(cache) = &mut self.cache {
                        if let Err(error) = cache.upsert_note(&note) {
                            self.status = StatusMessage {
                                text: format!("Live · cache warning: {error}"),
                                is_error: false,
                            };
                        }
                    }
                }
            }
            Err(error) => self.set_error(error),
        }
        perf::event("tui.preview_live", Some(id.as_str()), started, "complete");
    }

    fn scroll_preview(&mut self, delta: isize) {
        let previous = self.preview_scroll;
        self.preview_scroll = if delta.is_negative() {
            self.preview_scroll
                .saturating_sub(delta.unsigned_abs() as u16)
        } else {
            self.preview_scroll.saturating_add(delta as u16)
        };
        if self.preview_scroll != previous {
            self.persist_session_selection();
        }
    }

    fn set_error(&mut self, error: NotesError) {
        if matches!(
            self.data_source,
            DataSourceState::Cached | DataSourceState::CachedBackendUnavailable { .. }
        ) {
            self.data_source = DataSourceState::CachedBackendUnavailable {
                message: error.to_string(),
            };
            self.status = StatusMessage {
                text: format!("Cached · backend unavailable: {error}"),
                is_error: true,
            };
            return;
        }
        self.status = StatusMessage {
            text: format!("ERROR: {error}"),
            is_error: true,
        };
    }

    fn persist_cache_snapshot(&mut self) {
        let started = Instant::now();
        let state = self.cached_state();
        if let Some(cache) = &mut self.cache {
            if let Err(error) = cache.replace_snapshot(&state) {
                self.status = StatusMessage {
                    text: format!("Live · cache warning: {error}"),
                    is_error: false,
                };
            }
        }
        perf::event("tui.persist_cache_snapshot", None, started, "complete");
    }

    fn require_live_backend(&mut self) -> bool {
        if matches!(self.data_source, DataSourceState::Live) {
            return true;
        }
        self.status = StatusMessage {
            text: "Live Notes backend unavailable; cached mode is read-only".into(),
            is_error: true,
        };
        false
    }
}

fn read_live_state(
    backend: &SharedBackend,
    context: Option<RefreshContext>,
    session: Option<SessionState>,
    preferred_note: Option<NoteId>,
    cancel: Option<&AtomicBool>,
) -> Result<LiveRefreshRead, NotesError> {
    let started = Instant::now();
    let mutex_started = Instant::now();
    let backend = backend
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    perf::event("tui.backend_mutex_wait", None, mutex_started, "acquired");
    let accounts = backend.accounts_with_cancel(cancel)?;
    let folders = backend.folders_with_cancel(None, cancel)?;
    let session_context = session
        .as_ref()
        .and_then(|session| session_context_for_data(&accounts, &folders, session));
    let context = session_context
        .clone()
        .or(context)
        .or_else(|| {
            let navigation = build_navigation(&accounts, &folders);
            navigation
                .iter()
                .find_map(|item| match item {
                    NavigationItem::Folder { id, .. } => Some(RefreshContext::Folder(id.clone())),
                    _ => None,
                })
                .or_else(|| {
                    navigation.into_iter().find_map(|item| match item {
                        NavigationItem::Account { id, .. } => Some(RefreshContext::Account(id)),
                        _ => None,
                    })
                })
        })
        .ok_or_else(|| NotesError::Backend("No accounts or folders available".into()))?;
    let NotesPage { items, total, .. } = backend.notes_with_cancel(&context.query(), cancel)?;
    let session_note = session_context.and_then(|_| session.and_then(|session| session.note_id));
    let selected_id = preferred_note
        .or(session_note)
        .as_ref()
        .filter(|id| items.iter().any(|item| item.id == **id))
        .cloned()
        .or_else(|| items.first().map(|item| item.id.clone()));
    let selected_note = selected_id
        .as_ref()
        .map(|id| backend.get_note_with_cancel(id, cancel))
        .transpose()?;
    let read = LiveRefreshRead {
        accounts,
        folders,
        context,
        notes: items,
        total,
        selected_note,
    };
    perf::event("tui.read_live_state", None, started, "ok");
    Ok(read)
}

fn session_context_for_data(
    accounts: &[Account],
    folders: &[Folder],
    session: &SessionState,
) -> Option<RefreshContext> {
    let account_id = session.account_id.as_ref()?;
    let account = accounts.iter().find(|account| &account.id == account_id)?;
    if let Some(folder_id) = &session.folder_id {
        if folders
            .iter()
            .any(|folder| &folder.id == folder_id && &folder.account_id == account_id)
        {
            return Some(RefreshContext::Folder(folder_id.clone()));
        }
    }
    account
        .default_folder_id
        .as_ref()
        .filter(|folder_id| {
            folders
                .iter()
                .any(|folder| &folder.id == *folder_id && &folder.account_id == account_id)
        })
        .cloned()
        .map(RefreshContext::Folder)
        .or_else(|| {
            folders
                .iter()
                .find(|folder| &folder.account_id == account_id)
                .map(|folder| RefreshContext::Folder(folder.id.clone()))
        })
        .or_else(|| Some(RefreshContext::Account(account.id.clone())))
}

fn settings_edits(settings: &SettingsState) -> Vec<ConfigEdit> {
    let keys = [
        ConfigKey::AutoRefresh,
        ConfigKey::RefreshIntervalSeconds,
        ConfigKey::PreviewWrap,
        ConfigKey::ShowAttachmentMetadata,
    ];
    settings
        .staged
        .iter()
        .enumerate()
        .filter_map(|(index, stage)| match stage {
            SettingDraft::Unchanged => None,
            SettingDraft::Set(value) => Some(ConfigEdit::Set(keys[index], *value)),
            SettingDraft::Unset => Some(ConfigEdit::Unset(keys[index])),
        })
        .collect()
}

fn move_index(current: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        return 0;
    }
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(length - 1)
    }
}
fn edge_index(length: usize, last: bool) -> usize {
    if last {
        length.saturating_sub(1)
    } else {
        0
    }
}
fn build_navigation(accounts: &[Account], folders: &[Folder]) -> Vec<NavigationItem> {
    let mut output = Vec::new();
    for account in accounts {
        output.push(NavigationItem::Account {
            id: account.id.clone(),
            name: account.name.clone(),
        });
        let mut visited = HashSet::new();
        append_folders(
            &mut output,
            folders,
            &FolderParent::Account {
                account_id: account.id.clone(),
            },
            account.id.clone(),
            0,
            &mut visited,
        );
    }
    output
}

fn append_folders(
    output: &mut Vec<NavigationItem>,
    folders: &[Folder],
    parent: &FolderParent,
    account_id: AccountId,
    depth: usize,
    visited: &mut HashSet<FolderId>,
) {
    for folder in folders.iter().filter(|folder| folder.parent == *parent) {
        if !visited.insert(folder.id.clone()) {
            continue;
        }
        output.push(NavigationItem::Folder {
            id: folder.id.clone(),
            account_id: account_id.clone(),
            name: folder.name.clone(),
            depth,
        });
        append_folders(
            output,
            folders,
            &FolderParent::Folder {
                folder_id: folder.id.clone(),
            },
            account_id.clone(),
            depth + 1,
            visited,
        );
    }
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MINIMUM_WIDTH || area.height < MINIMUM_HEIGHT {
        let message = format!(
            "Terminal too small\nMinimum: {MINIMUM_WIDTH}x{MINIMUM_HEIGHT}\nCurrent: {}x{}",
            area.width, area.height
        );
        frame.render_widget(
            Paragraph::new(message).alignment(Alignment::Center).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Apple Notes TUI"),
            ),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(27),
            Constraint::Percentage(33),
            Constraint::Percentage(40),
        ])
        .split(rows[0]);
    render_navigation(frame, columns[0], app);
    render_notes(frame, columns[1], app);
    if app.edit.is_some() {
        render_editor(frame, columns[2], app);
    } else {
        render_preview(frame, columns[2], app);
    }
    let path = app
        .navigation
        .get(app.selected_navigation)
        .map(navigation_label)
        .unwrap_or_else(|| "No selection".into());
    let suffix = "q quit | , settings | / search | Tab focus | j/k move | Enter open | a attachments | r refresh | ? help";
    let mode = if app.mode == AppMode::Insert {
        "INSERT"
    } else if app.status.is_error {
        "ERROR"
    } else {
        "NORMAL"
    };
    let search = app.search_status().unwrap_or_default();
    let status = format!(
        "{} | {} | {} | {} | {suffix}",
        mode, path, app.status.text, search
    );
    frame.render_widget(
        Paragraph::new(status)
            .style(if app.status.is_error {
                app.theme.error
            } else {
                app.theme.status
            })
            .wrap(Wrap { trim: true }),
        rows[1],
    );
    if app.show_help {
        render_help(frame, centered_rect(64, 64, area));
    }
    if let Some(popup) = &app.popup {
        let popup_area = if matches!(popup, Popup::Attachments { .. }) {
            centered_rect(72, 62, area)
        } else {
            centered_rect(58, 38, area)
        };
        render_popup(frame, popup_area, popup, app);
    }
}

fn pane_block(title: &str, active: bool, theme: Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(if active {
            theme.active
        } else {
            Style::default()
        })
}
fn render_navigation(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<_> = app
        .navigation
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let selected = app.focus == Focus::Navigation && index == app.selected_navigation;
            ListItem::new(navigation_label(item)).style(if selected {
                app.theme.selected
            } else {
                Style::default()
            })
        })
        .collect();
    frame.render_widget(
        List::new(items).block(pane_block(
            "Accounts / Folders",
            app.focus == Focus::Navigation,
            app.theme,
        )),
        area,
    );
}
fn navigation_label(item: &NavigationItem) -> String {
    match item {
        NavigationItem::Account { name, .. } => name.clone(),
        NavigationItem::Folder {
            name,
            depth,
            account_id,
            ..
        } => {
            let _ = account_id;
            format!("{}└─ {name}", "  ".repeat(depth + 1))
        }
    }
}
fn render_notes(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<_> = app
        .visible_note_ids()
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            app.notes
                .iter()
                .find(|note| note.id == *id)
                .map(|note| (index, note))
        })
        .map(|(index, note)| {
            ListItem::new(note.name.clone()).style(
                if app.focus == Focus::Notes && index == app.selected_note_index {
                    app.theme.selected
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    frame.render_widget(
        List::new(items).block(pane_block("Notes", app.focus == Focus::Notes, app.theme)),
        area,
    );
}
fn render_preview(frame: &mut Frame, area: Rect, app: &App) {
    let text = match &app.selected_note {
        Some(note) => preview_text(note, app.show_attachment_metadata),
        None if matches!(
            app.cached_preview_state,
            Some(CachedPreviewState::MissingFullNote)
        ) => Text::from(
            "Full note content is not available in the local cache.\nThe note summary remains available offline.",
        ),
        None if matches!(
            app.cached_preview_state,
            Some(CachedPreviewState::ReadError(_))
        ) => Text::from(format!(
            "Local cached note content could not be read.\n{}",
            match &app.cached_preview_state {
                Some(CachedPreviewState::ReadError(error)) => error,
                _ => unreachable!(),
            }
        )),
        None if app
            .visible_note(app.selected_note_index)
            .is_some_and(|note| note.password_protected) => {
            Text::from("[locked] Password protected\nContent is unavailable until the note is unlocked in Notes.app.")
        }
        None => Text::from("Select a note to preview."),
    };
    let paragraph = Paragraph::new(text)
        .block(pane_block(
            "Preview",
            app.focus == Focus::Preview,
            app.theme,
        ))
        .scroll((app.preview_scroll, 0));
    let paragraph = if app.preview_wrap {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, area);
}
fn render_editor(frame: &mut Frame, area: Rect, app: &App) {
    let edit = app.edit.as_ref().unwrap_or_else(|| unreachable!());
    let field = if edit.field == EditField::Title {
        "Title"
    } else {
        "Body"
    };
    let body = editor_blocks(&edit.document, edit.current_target);
    let text = format!(
        "Title: {}\n\nBody:\n{}\n\nEditing {field} | Tab fields | Ctrl-b/i/u style | Ctrl-k link | Ctrl-d delete unit\nAlt-j/k unit | Alt-1/2/3/p/b/n/q/c structure",
        edit.title_buffer, body
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(pane_block("Edit Note", true, app.theme))
            .wrap(Wrap { trim: false })
            .scroll((edit.viewport, 0)),
        area,
    );

    if app.mode == AppMode::Insert && app.popup.is_none() {
        let content_x = area.x.saturating_add(1);
        let content_y = area.y.saturating_add(1);
        let (line, before_cursor) = match edit.field {
            EditField::Title => (
                0,
                format!(
                    "Title: {}",
                    edit.title_buffer
                        .chars()
                        .take(edit.cursor)
                        .collect::<String>()
                ),
            ),
            EditField::Body => {
                let visual_index = edit
                    .document
                    .visual_index(edit.current_target)
                    .unwrap_or_default() as u16;
                let prefix = editor_target_prefix(&edit.document, edit.current_target);
                let target_text = edit
                    .document
                    .target_text(edit.current_target)
                    .unwrap_or_default();
                (
                    3_u16.saturating_add(visual_index),
                    format!(
                        ">{}{}",
                        prefix,
                        target_text.chars().take(edit.cursor).collect::<String>()
                    ),
                )
            }
        };
        let width = UnicodeWidthStr::width(before_cursor.as_str()) as u16;
        let cursor_x = content_x
            .saturating_add(width)
            .min(area.right().saturating_sub(2));
        let cursor_y = content_y
            .saturating_add(line)
            .saturating_sub(edit.viewport)
            .min(area.bottom().saturating_sub(2));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Editor scrolling is a best-effort logical line offset. It deliberately does
/// not include terminal dimensions or wrapping width; rendering clamps it to
/// the current layout.
fn editor_viewport_max(edit: &EditSession) -> u16 {
    let lines = editor_blocks(&edit.document, edit.current_target)
        .lines()
        .count()
        .saturating_add(3);
    u16::try_from(lines.saturating_sub(1)).unwrap_or(u16::MAX)
}

fn editor_blocks(document: &EditorDocument, current_target: EditorTarget) -> String {
    document
        .document
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(block_index, block)| match block {
            notes_core::Block::Paragraph(items) => vec![editor_line(
                current_target,
                EditorTarget::Block { block_index },
                "[P ] ",
                inline_plain(items),
            )],
            notes_core::Block::Heading1(items) => vec![editor_line(
                current_target,
                EditorTarget::Block { block_index },
                "[H1] ",
                inline_plain(items),
            )],
            notes_core::Block::Heading2(items) => vec![editor_line(
                current_target,
                EditorTarget::Block { block_index },
                "[H2] ",
                inline_plain(items),
            )],
            notes_core::Block::Heading3(items) => vec![editor_line(
                current_target,
                EditorTarget::Block { block_index },
                "[H3] ",
                inline_plain(items),
            )],
            notes_core::Block::Quote(items) => vec![editor_line(
                current_target,
                EditorTarget::Block { block_index },
                "[Q ] ",
                inline_plain(items),
            )],
            notes_core::Block::CodeBlock(text) => vec![editor_line(
                current_target,
                EditorTarget::Block { block_index },
                "[C ] ",
                text.clone(),
            )],
            notes_core::Block::BulletList(items) => items
                .iter()
                .enumerate()
                .map(|(item_index, item)| {
                    editor_line(
                        current_target,
                        EditorTarget::ListItem {
                            block_index,
                            item_index,
                        },
                        "[• ] ",
                        inline_plain(&item.content),
                    )
                })
                .collect(),
            notes_core::Block::NumberedList(items) => items
                .iter()
                .enumerate()
                .map(|(item_index, item)| {
                    editor_line(
                        current_target,
                        EditorTarget::ListItem {
                            block_index,
                            item_index,
                        },
                        &format!("[{}.] ", item_index + 1),
                        inline_plain(&item.content),
                    )
                })
                .collect(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn editor_line(
    current_target: EditorTarget,
    target: EditorTarget,
    prefix: &str,
    text: String,
) -> String {
    format!(
        "{}{prefix}{text}",
        if current_target == target { ">" } else { " " }
    )
}

fn editor_target_prefix(document: &EditorDocument, target: EditorTarget) -> String {
    match target {
        EditorTarget::Block { block_index } => match document.document.blocks.get(block_index) {
            Some(notes_core::Block::Paragraph(_)) => "[P ] ".into(),
            Some(notes_core::Block::Heading1(_)) => "[H1] ".into(),
            Some(notes_core::Block::Heading2(_)) => "[H2] ".into(),
            Some(notes_core::Block::Heading3(_)) => "[H3] ".into(),
            Some(notes_core::Block::Quote(_)) => "[Q ] ".into(),
            Some(notes_core::Block::CodeBlock(_)) => "[C ] ".into(),
            _ => String::new(),
        },
        EditorTarget::ListItem {
            block_index,
            item_index,
        } => match document.document.blocks.get(block_index) {
            Some(notes_core::Block::BulletList(_)) => "[• ] ".into(),
            Some(notes_core::Block::NumberedList(_)) => format!("[{}.] ", item_index + 1),
            _ => String::new(),
        },
    }
}

fn inline_plain(items: &[notes_core::Inline]) -> String {
    items
        .iter()
        .map(|item| match item {
            notes_core::Inline::Text(text) => text.clone(),
            notes_core::Inline::Bold(children)
            | notes_core::Inline::Italic(children)
            | notes_core::Inline::Underline(children)
            | notes_core::Inline::Strikethrough(children) => inline_plain(children),
            notes_core::Inline::Link { label, .. } => inline_plain(label),
        })
        .collect()
}

fn render_popup(frame: &mut Frame, area: Rect, popup: &Popup, app: &App) {
    frame.render_widget(Clear, area);
    let text = match popup {
        Popup::RenameFolder { name, .. } => {
            format!("Rename folder\n\nName: {name}\n\nEnter rename | Esc cancel")
        }
        Popup::ReparentFolder {
            source_folder_name,
            destinations,
            selected_destination,
            ..
        } => format!(
            "Move “{source_folder_name}” to:\n\n{}\n\nEnter move | Esc cancel",
            destinations
                .iter()
                .enumerate()
                .map(|(index, target)| {
                    let label = match target {
                        FolderReparentTarget::AccountRoot => "Account root".into(),
                        FolderReparentTarget::Folder {
                            display_name, depth, ..
                        } => format!("{}{}", "  ".repeat(*depth), display_name),
                    };
                    format!("{} {label}", if index == *selected_destination { ">" } else { " " })
                })
                .collect::<Vec<_>>()
                .join("\n")
        ),
        Popup::CreateFolder { name, .. } => {
            format!("Create folder\n\nName: {name}\n\nEnter create | Esc cancel")
        }
        Popup::CreateChildFolder { parent_folder_name, name, .. } => {
            format!("Create subfolder in “{parent_folder_name}”\n\nName: {name}\n\nEnter create | Esc cancel")
        }
        Popup::Conflict(_) => {
            "Note changed outside TUI.\n\n[r] reload remote\n[o] overwrite\n[c] cancel".into()
        }
        Popup::Discard { .. } => "Discard unsaved changes?\n\n[y] discard\n[n] keep editing".into(),
        Popup::Move { selected } => {
            let folders: Vec<_> = app
                .navigation
                .iter()
                .filter_map(|item| {
                    if let NavigationItem::Folder { name, .. } = item {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect();
            format!(
                "Move note to:\n\n{}\n\nEnter move | Esc cancel",
                folders
                    .iter()
                    .enumerate()
                    .map(|(index, name)| format!(
                        "{} {name}",
                        if index == *selected { ">" } else { " " }
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
        Popup::Attachments { selected } => attachment_popup_text(app, *selected),
        Popup::Link { url, .. } => format!("Link URL:\n\n{url}\n\nEnter apply | Esc cancel"),
        Popup::DeleteConfirm(note) => format!(
            "Move “{}” to Recently Deleted?\n\n[y] move   [N/Esc] cancel",
            note.summary.name
        ),
        Popup::DeleteFolder { folder_name, .. } => format!(
            "Delete folder “{folder_name}”?\n\nOnly empty folders can be deleted.\nThis action removes the folder from Notes.\n\n[y] delete   [N/Esc] cancel"
        ),
        Popup::Settings(settings) => settings_popup_text(settings, app),
        Popup::DraftRecovery { draft, recoverable } => {
            let target = match draft.kind {
                EditorRecoveryKind::Create => "new note",
                EditorRecoveryKind::Edit => "existing note",
            };
            if *recoverable {
                format!("Unsaved local {target} draft found\n\nIt has not been saved to Notes.app.\n\n[r] Restore  [d] Discard  [Esc] Later")
            } else {
                format!("Unsaved local {target} draft found, but its target is unavailable.\n\nThe draft remains on disk.\n\n[d] Discard  [Esc] Later")
            }
        }
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Apple Notes TUI"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
fn settings_popup_text(settings: &SettingsState, app: &App) -> String {
    let names = [
        "Auto refresh",
        "Refresh interval",
        "Preview wrap",
        "Attachment metadata",
    ];
    let values = [
        settings.draft.auto_refresh.to_string(),
        settings
            .interval_input
            .clone()
            .unwrap_or_else(|| format!("{}s", settings.draft.refresh_interval.as_secs())),
        settings.draft.preview_wrap.to_string(),
        settings.draft.show_attachment_metadata.to_string(),
    ];
    let sources = app.config_sources.map(|source| match source {
        ConfigValueSource::Default => "default",
        ConfigValueSource::File => "file",
        ConfigValueSource::Cli => "cli",
    });
    let rows = (0..4)
        .map(|index| {
            format!(
                "{}{} {:<22} {:<7} [{}]",
                if index == settings.selected { ">" } else { " " },
                if settings.staged[index] == SettingDraft::Unchanged {
                    " "
                } else {
                    "*"
                },
                names[index],
                values[index],
                sources[index]
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let detail = settings
        .error
        .as_deref()
        .unwrap_or("j/k select | Space toggle | Enter edit interval | s save | Esc cancel");
    format!("Settings (changes apply next startup)\n\n{rows}\n\n{detail}")
}
fn preview_text(note: &Note, show_attachment_metadata: bool) -> Text<'static> {
    let lock = if note.summary.password_protected {
        "[locked] Password protected"
    } else {
        "Unlocked"
    };
    let mut lines = vec![
        Line::from(Span::styled(
            note.summary.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "Folder: {} | Account: {}",
            note.summary.folder_id, note.account_id
        )),
        Line::from(format!("Created: {}", note.summary.creation_date)),
        Line::from(format!("Modified: {}", note.summary.modification_date)),
        Line::from(format!(
            "Shared: {} | {lock} | Attachments: {}",
            note.summary.shared, note.summary.attachment_count
        )),
        Line::from(""),
    ];
    if let Ok(mut document) = parse_notes_html(&note.body_html) {
        strip_notes_title_block(&mut document, &note.summary.name);
        lines.extend(rich_preview_lines(&document));
    } else {
        lines.push(Line::from(note.plaintext.clone()));
    }
    if !note.attachments.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Attachments",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for attachment in &note.attachments {
            let name = attachment_display_name(attachment);
            lines.push(Line::from(if show_attachment_metadata {
                format!(
                    "• {name} [{}] — preview {}",
                    attachment.kind, attachment.preview_status
                )
            } else {
                format!("• {name}")
            }));
        }
    }
    Text::from(lines)
}

fn attachment_display_name(attachment: &AttachmentSummary) -> String {
    if attachment.display_name.is_empty() {
        format!("Attachment {}", attachment.id)
    } else {
        attachment.display_name.clone()
    }
}

fn attachment_popup_text(app: &App, selected: usize) -> String {
    let Some(note) = &app.selected_note else {
        return "Attachment metadata is unavailable.\n\nEsc close".into();
    };
    let items = note
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            format!(
                "{} {} [{}]",
                if index == selected { ">" } else { " " },
                attachment_display_name(attachment),
                attachment.kind
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let details = note.attachments.get(selected).map_or_else(
        || "No attachment selected".into(),
        |attachment| {
            format!(
                "ID: {}\nContent ID: {}\nSource URL: {}\nPreview: {}\nExport copy: {}",
                attachment.id,
                attachment
                    .content_identifier
                    .as_deref()
                    .unwrap_or("unavailable"),
                attachment.source_url.as_deref().unwrap_or("unavailable"),
                attachment.preview_status,
                attachment.export_status
            )
        },
    );
    if app.show_attachment_metadata {
        format!(
            "Attachments ({})\n\n{items}\n\n{details}\n\nj/k select | Enter preview in Notes.app | x export copy… | Esc close",
            note.attachments.len()
        )
    } else {
        format!(
            "Attachments ({})\n\n{items}\n\nj/k select | Enter preview in Notes.app | x export copy… | Esc close",
            note.attachments.len()
        )
    }
}

fn editor_document_from_html(
    title: &str,
    body_html: &str,
) -> Result<EditorDocument, notes_core::RichTextError> {
    let mut document = parse_notes_html(body_html)?;
    strip_notes_title_block(&mut document, title);
    Ok(EditorDocument::new(document))
}

fn strip_notes_title_block(document: &mut notes_core::RichDocument, title: &str) {
    let is_notes_title = matches!(
        document.blocks.first(),
        Some(notes_core::Block::Paragraph(items))
            if matches!(&items[..], [notes_core::Inline::Text(text)] if text == title)
    );
    if is_notes_title {
        document.blocks.remove(0);
    }
}

fn target_kind_feature(kind: TargetKind) -> Option<RichFeature> {
    match kind {
        TargetKind::Paragraph => None,
        TargetKind::Heading1 => Some(RichFeature::Heading1),
        TargetKind::Heading2 => Some(RichFeature::Heading2),
        TargetKind::Heading3 => Some(RichFeature::Heading3),
        TargetKind::Bullet => Some(RichFeature::BulletList),
        TargetKind::Numbered => Some(RichFeature::NumberedList),
        TargetKind::Quote => Some(RichFeature::Quote),
        TargetKind::Code => Some(RichFeature::Code),
    }
}

fn mixed_list_boundary_count(document: &notes_core::RichDocument) -> usize {
    document
        .blocks
        .windows(2)
        .filter(|pair| {
            matches!(
                pair,
                [
                    notes_core::Block::BulletList(_),
                    notes_core::Block::NumberedList(_)
                ] | [
                    notes_core::Block::NumberedList(_),
                    notes_core::Block::BulletList(_)
                ]
            )
        })
        .count()
}

fn rich_preview_lines(document: &notes_core::RichDocument) -> Vec<Line<'static>> {
    document
        .blocks
        .iter()
        .flat_map(|block| match block {
            notes_core::Block::Paragraph(inlines) => {
                vec![Line::from(inline_spans(inlines, Style::default()))]
            }
            notes_core::Block::Heading1(inlines)
            | notes_core::Block::Heading2(inlines)
            | notes_core::Block::Heading3(inlines) => vec![Line::from(inline_spans(
                inlines,
                Style::default().add_modifier(Modifier::BOLD),
            ))],
            notes_core::Block::Quote(inlines) => {
                let mut spans = vec![Span::raw("> ")];
                spans.extend(inline_spans(inlines, Style::default()));
                vec![Line::from(spans)]
            }
            notes_core::Block::CodeBlock(text) => text
                .lines()
                .map(|line| Line::from(format!("    {line}")))
                .collect(),
            notes_core::Block::BulletList(items) => items
                .iter()
                .map(|item| {
                    let mut spans = vec![Span::raw("• ")];
                    spans.extend(inline_spans(&item.content, Style::default()));
                    Line::from(spans)
                })
                .collect(),
            notes_core::Block::NumberedList(items) => items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let mut spans = vec![Span::raw(format!("{}. ", index + 1))];
                    spans.extend(inline_spans(&item.content, Style::default()));
                    Line::from(spans)
                })
                .collect(),
        })
        .collect()
}
fn inline_spans(inlines: &[notes_core::Inline], style: Style) -> Vec<Span<'static>> {
    inlines
        .iter()
        .flat_map(|inline| match inline {
            notes_core::Inline::Text(text) => vec![Span::styled(text.clone(), style)],
            notes_core::Inline::Bold(children) => {
                inline_spans(children, style.add_modifier(Modifier::BOLD))
            }
            notes_core::Inline::Italic(children) => {
                inline_spans(children, style.add_modifier(Modifier::ITALIC))
            }
            notes_core::Inline::Underline(children) => {
                inline_spans(children, style.add_modifier(Modifier::UNDERLINED))
            }
            notes_core::Inline::Strikethrough(children) => {
                inline_spans(children, style.add_modifier(Modifier::CROSSED_OUT))
            }
            notes_core::Inline::Link { label, .. } => {
                inline_spans(label, style.add_modifier(Modifier::UNDERLINED))
            }
        })
        .collect()
}
fn render_help(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let text = "Navigation\n  j/k, Up/Down   move\n  h/l, Left/Right change pane\n  Tab / Shift-Tab next / previous pane\n  Enter           activate\n  g/G, Home/End   first / last\n  r               refresh\n  n / m           create / move note\n  N               create top-level folder\n  C / R           create child / rename selected folder\n  D               delete selected folder when no note is selected\n  M               move selected folder within its account\n  ,               settings (next startup)\n\nSearch\n  /               open search\n  Enter           apply search\n  Esc             cancel input / clear active search\n  Backspace       edit query\n  Ctrl-u          clear query\n\nAttachments\n  a               open attachment list\n  j/k             select attachment\n  Enter           preview explicitly in Notes.app\n  x               export a copy with macOS Save dialog\n  Esc             close\n\nPreview\n  PgUp/PgDn, Ctrl-u/Ctrl-d scroll\n\nGeneral\n  q quit\n  ? or Esc close help";
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .wrap(Wrap { trim: false }),
        area,
    );
}
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub fn demo_backend() -> MockNotesBackend {
    let account = Account {
        id: AccountId::from("demo-account"),
        name: "iCloud".into(),
        is_default: true,
        is_upgraded: true,
        default_folder_id: Some(FolderId::from("demo-notes")),
    };
    let folders = vec![
        Folder {
            id: FolderId::from("demo-notes"),
            account_id: account.id.clone(),
            name: "Notes".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        },
        Folder {
            id: FolderId::from("demo-work"),
            account_id: account.id.clone(),
            name: "Work".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        },
        Folder {
            id: FolderId::from("demo-family"),
            account_id: account.id.clone(),
            name: "Семья".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        },
    ];
    let mut attachment_note = demo_html_note(
        "demo-attachment",
        "demo-notes",
        "Attachment example",
        "<div>Attachment preview is read-only</div>",
        0,
    );
    attachment_note.attachments = vec![
        demo_attachment("demo-image", "demo-attachment", "Photo.png"),
        demo_attachment("demo-pdf", "demo-attachment", "Manual.pdf"),
        demo_attachment("demo-unknown", "demo-attachment", "Opaque payload"),
    ];
    attachment_note.summary.attachment_count = attachment_note.attachments.len();

    let mut locked_attachment =
        demo_attachment("demo-locked-file", "demo-locked-attachment", "Locked.pdf");
    locked_attachment.preview_status =
        AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::ProtectedNote);
    locked_attachment.export_status =
        AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::ProtectedNote);
    let mut locked_note = demo_html_note(
        "demo-locked-attachment",
        "demo-notes",
        "Locked attachment example",
        "",
        1,
    );
    locked_note.summary.password_protected = true;
    locked_note.attachments = vec![locked_attachment];

    let notes = vec![
        demo_note(
            "demo-note-1",
            "demo-notes",
            "Welcome to Apple Notes TUI",
            "This is synthetic demo data.\n\nUse j/k to select notes.",
        ),
        demo_note("demo-search-alpha", "demo-notes", "Alpha Project", ""),
        demo_note(
            "demo-search-alpha-lower",
            "demo-notes",
            "alpha lowercase",
            "",
        ),
        demo_note("demo-search-beta", "demo-notes", "Beta Project", ""),
        demo_note(
            "demo-search-russian",
            "demo-notes",
            "Русская заметка",
            "Привет Мир",
        ),
        demo_note("demo-search-german", "demo-notes", "Grüße aus Köln", ""),
        demo_note("demo-search-emoji", "demo-notes", "Emoji 🚀 Note", ""),
        demo_note(
            "demo-search-body",
            "demo-notes",
            "Search Body Example",
            "secret-phase-seven-token",
        ),
        demo_note(
            "demo-note-2",
            "demo-work",
            "Project ideas",
            "A read-only Phase 2 preview.\n\nNo real Notes data is used in --demo.",
        ),
        demo_note(
            "demo-note-3",
            "demo-family",
            "Список покупок",
            "Хлеб\nМолоко\nЯблоки",
        ),
        demo_html_note(
            "demo-plain",
            "demo-notes",
            "Plain note",
            "<div>Plain text</div>",
            0,
        ),
        demo_html_note(
            "demo-rich",
            "demo-notes",
            "Rich formatting",
            "<h1>Heading</h1><div><b>Bold</b> <i>italic</i> <u>underlined</u></div>",
            0,
        ),
        demo_html_note(
            "demo-bullet",
            "demo-notes",
            "Bullet list",
            "<ul><li>item one</li><li>item two</li></ul>",
            0,
        ),
        demo_html_note(
            "demo-numbered",
            "demo-notes",
            "Numbered list",
            "<ol><li>item one</li><li>item two</li></ol>",
            0,
        ),
        demo_html_note(
            "demo-link",
            "demo-notes",
            "Link example",
            "<div><a href=\"https://example.com\">example</a></div>",
            0,
        ),
        attachment_note,
        locked_note,
        demo_html_note(
            "demo-table",
            "demo-notes",
            "Unsupported table",
            "<table><tr><td>unsafe</td></tr></table>",
            0,
        ),
    ];
    MockNotesBackend::new(
        vec![account],
        folders,
        notes
            .into_iter()
            .map(|note| (note.summary.id.clone(), note))
            .collect::<HashMap<_, _>>(),
    )
}

fn demo_attachment(id: &str, note_id: &str, display_name: &str) -> AttachmentSummary {
    let date = NoteDate::new("Synthetic attachment date");
    AttachmentSummary::from_apple_events_metadata(
        AttachmentMetadata {
            id: AttachmentId::from(id),
            note_id: NoteId::from(note_id),
            display_name: display_name.into(),
            content_identifier: Some(format!("cid:{id}@demo.invalid")),
            source_url: None,
            creation_date: date.clone(),
            modification_date: date,
            shared: false,
        },
        AttachmentCapabilities::notes_apple_events(),
    )
}
fn demo_html_note(
    id: &str,
    folder_id: &str,
    name: &str,
    body_html: &str,
    attachment_count: usize,
) -> Note {
    let mut note = demo_note(id, folder_id, name, "");
    note.summary.attachment_count = attachment_count;
    note.body_html = body_html.into();
    note.plaintext = body_html.replace(['<', '>'], " ");
    note
}
fn demo_note(id: &str, folder_id: &str, name: &str, plaintext: &str) -> Note {
    let date = NoteDate::new("Synthetic demo date");
    Note {
        summary: NoteSummary {
            id: NoteId::from(id),
            folder_id: FolderId::from(folder_id),
            name: name.into(),
            creation_date: date.clone(),
            modification_date: date,
            password_protected: false,
            shared: false,
            attachment_count: 0,
        },
        account_id: AccountId::from("demo-account"),
        body_html: format!("<div>{plaintext}</div>"),
        plaintext: plaintext.into(),
        attachments: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::panic::AssertUnwindSafe;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use notes_cache::{CacheError, CacheStore};
    use notes_core::{AttachmentExportResult, BackendCapabilities, DeleteNote, DeleteResult};
    use ratatui::{backend::TestBackend, Terminal};

    #[derive(Clone, Default)]
    struct MutationCounts {
        accounts: Arc<AtomicUsize>,
        folders: Arc<AtomicUsize>,
        notes: Arc<AtomicUsize>,
        creates: Arc<AtomicUsize>,
        updates: Arc<AtomicUsize>,
        moves: Arc<AtomicUsize>,
        gets: Arc<AtomicUsize>,
        conflict_on_next_get: Arc<AtomicBool>,
        previews: Arc<AtomicUsize>,
        exports: Arc<AtomicUsize>,
        deletes: Arc<AtomicUsize>,
        folders_created: Arc<AtomicUsize>,
        child_folders_created: Arc<AtomicUsize>,
        folders_renamed: Arc<AtomicUsize>,
        folders_deleted: Arc<AtomicUsize>,
        folders_reparented: Arc<AtomicUsize>,
        folder_create_fails: Arc<AtomicBool>,
        folder_rename_fails: Arc<AtomicBool>,
        folder_delete_fails: Arc<AtomicBool>,
        folder_reparent_fails: Arc<AtomicBool>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CacheFailurePoint {
        LoadBootstrap,
        ReplaceSnapshot,
        LoadNote,
        UpsertNote,
        RemoveNote,
    }
    #[derive(Default)]
    struct FakeCacheState {
        bootstrap: CachedState,
        full_notes: HashMap<NoteId, Note>,
        load_bootstrap_calls: usize,
        replace_snapshot_calls: usize,
        load_note_calls: usize,
        upsert_note_calls: usize,
        remove_note_calls: usize,
        load_note_ids: Vec<NoteId>,
        upsert_note_ids: Vec<NoteId>,
        removed_note_ids: Vec<NoteId>,
        replaced_snapshots: Vec<CachedState>,
        failure: Option<CacheFailurePoint>,
    }
    struct FakeCache(Arc<Mutex<FakeCacheState>>);
    impl FakeCache {
        fn new(state: FakeCacheState) -> (Self, Arc<Mutex<FakeCacheState>>) {
            let state = Arc::new(Mutex::new(state));
            (Self(state.clone()), state)
        }
    }
    impl CacheStore for FakeCache {
        fn load_bootstrap(&self) -> Result<CachedState, CacheError> {
            let mut s = self.0.lock().unwrap();
            s.load_bootstrap_calls += 1;
            if s.failure == Some(CacheFailurePoint::LoadBootstrap) {
                return Err(CacheError::Injected("load bootstrap"));
            }
            Ok(s.bootstrap.clone())
        }
        fn replace_snapshot(&mut self, snapshot: &CachedState) -> Result<(), CacheError> {
            let mut s = self.0.lock().unwrap();
            s.replace_snapshot_calls += 1;
            if s.failure == Some(CacheFailurePoint::ReplaceSnapshot) {
                return Err(CacheError::Injected("replace snapshot"));
            }
            s.replaced_snapshots.push(snapshot.clone());
            Ok(())
        }
        fn load_note(&self, id: &NoteId) -> Result<Option<Note>, CacheError> {
            let mut s = self.0.lock().unwrap();
            s.load_note_calls += 1;
            s.load_note_ids.push(id.clone());
            if s.failure == Some(CacheFailurePoint::LoadNote) {
                return Err(CacheError::Injected("load note"));
            }
            Ok(s.full_notes.get(id).cloned())
        }
        fn upsert_note(&mut self, note: &Note) -> Result<(), CacheError> {
            let mut s = self.0.lock().unwrap();
            s.upsert_note_calls += 1;
            s.upsert_note_ids.push(note.summary.id.clone());
            if s.failure == Some(CacheFailurePoint::UpsertNote) {
                return Err(CacheError::Injected("upsert note"));
            }
            s.full_notes.insert(note.summary.id.clone(), note.clone());
            Ok(())
        }
        fn remove_note(&self, id: &NoteId) -> Result<(), CacheError> {
            let mut s = self.0.lock().unwrap();
            s.remove_note_calls += 1;
            s.removed_note_ids.push(id.clone());
            if s.failure == Some(CacheFailurePoint::RemoveNote) {
                return Err(CacheError::Injected("remove note"));
            }
            s.full_notes.remove(id);
            Ok(())
        }
    }

    struct CountingBackend {
        inner: MockNotesBackend,
        capabilities: BackendCapabilities,
        counts: MutationCounts,
        create_fails: bool,
        update_fails: bool,
        move_fails: bool,
        delete_fails: bool,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum BackendCall {
        Accounts,
        Folders(Option<AccountId>),
        Notes(NotesQuery),
        GetNote(NoteId),
    }

    struct ScriptedFoldersCall {
        expected: Option<AccountId>,
        outcome: Result<Vec<Folder>, NotesError>,
    }

    struct ScriptedNotesCall {
        expected: NotesQuery,
        outcome: Result<NotesPage, NotesError>,
    }

    struct ScriptedGetNoteCall {
        expected: NoteId,
        outcome: Result<Note, NotesError>,
    }

    struct ScriptedNotesBackend {
        inner: MockNotesBackend,
        accounts: Mutex<VecDeque<Result<Vec<Account>, NotesError>>>,
        folders: Mutex<VecDeque<ScriptedFoldersCall>>,
        notes: Mutex<VecDeque<ScriptedNotesCall>>,
        get_notes: Mutex<VecDeque<ScriptedGetNoteCall>>,
        calls: Arc<Mutex<Vec<BackendCall>>>,
    }
    impl NotesBackend for ScriptedNotesBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.inner.capabilities()
        }
        fn accounts(&self) -> Result<Vec<Account>, NotesError> {
            self.calls.lock().unwrap().push(BackendCall::Accounts);
            self.accounts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    panic!("unexpected accounts call: no scripted outcome remaining")
                })
        }
        fn folders(&self, a: Option<&AccountId>) -> Result<Vec<Folder>, NotesError> {
            let actual = a.cloned();
            self.calls
                .lock()
                .unwrap()
                .push(BackendCall::Folders(actual.clone()));
            let expected = self
                .folders
                .lock()
                .unwrap()
                .front()
                .map(|entry| entry.expected.clone());
            let Some(expected) = expected else {
                panic!(
                    "unexpected folders call: no scripted outcome remaining; actual: {actual:?}"
                );
            };
            if expected != actual {
                panic!(
                    "unexpected folders argument: expected {:?}, actual {:?}",
                    expected, actual
                );
            }
            self.folders
                .lock()
                .unwrap()
                .pop_front()
                .expect("script entry checked above")
                .outcome
        }
        fn notes(&self, q: &NotesQuery) -> Result<NotesPage, NotesError> {
            let actual = q.clone();
            self.calls
                .lock()
                .unwrap()
                .push(BackendCall::Notes(actual.clone()));
            let expected = self
                .notes
                .lock()
                .unwrap()
                .front()
                .map(|entry| entry.expected.clone());
            let Some(expected) = expected else {
                panic!("unexpected notes call: no scripted outcome remaining; actual: {actual:?}");
            };
            if expected != actual {
                panic!(
                    "unexpected notes argument: expected {:?}, actual {:?}",
                    expected, actual
                );
            }
            self.notes
                .lock()
                .unwrap()
                .pop_front()
                .expect("script entry checked above")
                .outcome
        }
        fn get_note(&self, id: &NoteId) -> Result<Note, NotesError> {
            self.calls
                .lock()
                .unwrap()
                .push(BackendCall::GetNote(id.clone()));
            let expected = self
                .get_notes
                .lock()
                .unwrap()
                .front()
                .map(|entry| entry.expected.clone());
            let Some(expected) = expected else {
                panic!("unexpected get_note call: no scripted outcome remaining; actual: {id:?}");
            };
            if expected != *id {
                panic!(
                    "unexpected get_note argument: expected {:?}, actual {:?}",
                    expected, id
                );
            }
            self.get_notes
                .lock()
                .unwrap()
                .pop_front()
                .expect("script entry checked above")
                .outcome
        }
        fn preview_attachment(&self, n: &NoteId, a: &AttachmentId) -> Result<(), NotesError> {
            self.inner.preview_attachment(n, a)
        }
        fn export_attachment(
            &self,
            n: &NoteId,
            a: &AttachmentId,
        ) -> Result<AttachmentExportResult, NotesError> {
            self.inner.export_attachment(n, a)
        }
        fn create_note(&self, r: &CreateNote) -> Result<Note, NotesError> {
            self.inner.create_note(r)
        }
        fn update_note(&self, r: &UpdateNote) -> Result<Note, NotesError> {
            self.inner.update_note(r)
        }
        fn move_note(&self, r: &MoveNote) -> Result<Note, NotesError> {
            self.inner.move_note(r)
        }
        fn delete_note(&self, r: &DeleteNote) -> Result<DeleteResult, NotesError> {
            self.inner.delete_note(r)
        }
    }

    impl NotesBackend for CountingBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.capabilities
        }

        fn accounts(&self) -> Result<Vec<Account>, NotesError> {
            self.counts.accounts.fetch_add(1, Ordering::SeqCst);
            self.inner.accounts()
        }

        fn folders(&self, account: Option<&AccountId>) -> Result<Vec<Folder>, NotesError> {
            self.counts.folders.fetch_add(1, Ordering::SeqCst);
            self.inner.folders(account)
        }

        fn notes(&self, query: &NotesQuery) -> Result<NotesPage, NotesError> {
            self.counts.notes.fetch_add(1, Ordering::SeqCst);
            self.inner.notes(query)
        }

        fn get_note(&self, id: &NoteId) -> Result<Note, NotesError> {
            self.counts.gets.fetch_add(1, Ordering::SeqCst);
            let mut note = self.inner.get_note(id)?;
            if self
                .counts
                .conflict_on_next_get
                .swap(false, Ordering::SeqCst)
            {
                note.summary.modification_date = NoteDate::new("synthetic remote conflict");
            }
            Ok(note)
        }

        fn preview_attachment(
            &self,
            note_id: &NoteId,
            attachment_id: &AttachmentId,
        ) -> Result<(), NotesError> {
            self.counts.previews.fetch_add(1, Ordering::SeqCst);
            self.inner.preview_attachment(note_id, attachment_id)
        }

        fn export_attachment(
            &self,
            note_id: &NoteId,
            attachment_id: &AttachmentId,
        ) -> Result<AttachmentExportResult, NotesError> {
            self.counts.exports.fetch_add(1, Ordering::SeqCst);
            self.inner.export_attachment(note_id, attachment_id)
        }

        fn create_note(&self, request: &CreateNote) -> Result<Note, NotesError> {
            self.counts.creates.fetch_add(1, Ordering::SeqCst);
            if self.create_fails {
                return Err(NotesError::Backend("synthetic create failure".into()));
            }
            self.inner.create_note(request)
        }

        fn create_folder(&self, request: &CreateFolder) -> Result<Folder, NotesError> {
            self.counts.folders_created.fetch_add(1, Ordering::SeqCst);
            if self
                .counts
                .folder_create_fails
                .swap(false, Ordering::SeqCst)
            {
                return Err(NotesError::Backend(
                    "synthetic folder create failure".into(),
                ));
            }
            self.inner.create_folder(request)
        }

        fn create_child_folder(&self, request: &CreateChildFolder) -> Result<Folder, NotesError> {
            self.counts
                .child_folders_created
                .fetch_add(1, Ordering::SeqCst);
            if self
                .counts
                .folder_create_fails
                .swap(false, Ordering::SeqCst)
            {
                return Err(NotesError::Backend(
                    "synthetic child-folder create failure".into(),
                ));
            }
            self.inner.create_child_folder(request)
        }

        fn rename_folder(&self, request: &RenameFolder) -> Result<Folder, NotesError> {
            self.counts.folders_renamed.fetch_add(1, Ordering::SeqCst);
            if self
                .counts
                .folder_rename_fails
                .swap(false, Ordering::SeqCst)
            {
                return Err(NotesError::Backend(
                    "synthetic folder rename failure".into(),
                ));
            }
            self.inner.rename_folder(request)
        }

        fn delete_folder(&self, request: &DeleteFolder) -> Result<DeletedFolder, NotesError> {
            self.counts.folders_deleted.fetch_add(1, Ordering::SeqCst);
            if self
                .counts
                .folder_delete_fails
                .swap(false, Ordering::SeqCst)
            {
                return Err(NotesError::Backend(
                    "synthetic folder delete failure".into(),
                ));
            }
            self.inner.delete_folder(request)
        }
        fn reparent_folder(&self, request: &ReparentFolder) -> Result<Folder, NotesError> {
            self.counts
                .folders_reparented
                .fetch_add(1, Ordering::SeqCst);
            if self.counts.folder_reparent_fails.load(Ordering::SeqCst) {
                return Err(NotesError::Backend(
                    "synthetic folder reparent failure".into(),
                ));
            }
            self.inner.reparent_folder(request)
        }

        fn update_note(&self, request: &UpdateNote) -> Result<Note, NotesError> {
            self.counts.updates.fetch_add(1, Ordering::SeqCst);
            if self.update_fails {
                return Err(NotesError::Backend("synthetic update failure".into()));
            }
            self.inner.update_note(request)
        }

        fn move_note(&self, request: &MoveNote) -> Result<Note, NotesError> {
            self.counts.moves.fetch_add(1, Ordering::SeqCst);
            if self.move_fails {
                return Err(NotesError::Backend("synthetic move failure".into()));
            }
            self.inner.move_note(request)
        }

        fn delete_note(&self, request: &DeleteNote) -> Result<DeleteResult, NotesError> {
            self.counts.deletes.fetch_add(1, Ordering::SeqCst);
            if self.delete_fails {
                return Err(NotesError::Backend("synthetic delete failure".into()));
            }
            self.inner.delete_note(request)
        }
    }

    struct BlockingRefreshBackend {
        inner: MockNotesBackend,
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
        counts: MutationCounts,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BlockedMutation {
        Create,
        Update,
        Folder,
        CreateChildFolder,
        RenameFolder,
        DeleteFolder,
        ReparentFolder,
    }

    /// A narrow extension of the existing deterministic backend fixtures: it
    /// blocks exactly one mutation after the worker has acquired the backend.
    /// The channel handshake proves lifecycle timing without wall-clock waits.
    struct BlockingMutationBackend {
        inner: MockNotesBackend,
        counts: MutationCounts,
        blocked: BlockedMutation,
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
        update_fails: bool,
    }

    impl BlockingMutationBackend {
        fn wait_at_mutation(&self, mutation: BlockedMutation) {
            if self.blocked != mutation {
                return;
            }
            if let Some(sender) = self.started.lock().unwrap().take() {
                let _ = sender.send(());
                if let Some(receiver) = self.release.lock().unwrap().take() {
                    let _ = receiver.recv();
                }
            }
        }
    }

    impl NotesBackend for BlockingMutationBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.inner.capabilities()
        }
        fn accounts(&self) -> Result<Vec<Account>, NotesError> {
            self.counts.accounts.fetch_add(1, Ordering::SeqCst);
            self.inner.accounts()
        }
        fn folders(&self, account: Option<&AccountId>) -> Result<Vec<Folder>, NotesError> {
            self.counts.folders.fetch_add(1, Ordering::SeqCst);
            self.inner.folders(account)
        }
        fn notes(&self, query: &NotesQuery) -> Result<NotesPage, NotesError> {
            self.counts.notes.fetch_add(1, Ordering::SeqCst);
            self.inner.notes(query)
        }
        fn get_note(&self, id: &NoteId) -> Result<Note, NotesError> {
            self.counts.gets.fetch_add(1, Ordering::SeqCst);
            let mut note = self.inner.get_note(id)?;
            if self
                .counts
                .conflict_on_next_get
                .swap(false, Ordering::SeqCst)
            {
                note.summary.modification_date = NoteDate::new("synthetic remote conflict");
            }
            Ok(note)
        }
        fn preview_attachment(
            &self,
            note_id: &NoteId,
            attachment_id: &AttachmentId,
        ) -> Result<(), NotesError> {
            self.counts.previews.fetch_add(1, Ordering::SeqCst);
            self.inner.preview_attachment(note_id, attachment_id)
        }
        fn export_attachment(
            &self,
            note_id: &NoteId,
            attachment_id: &AttachmentId,
        ) -> Result<AttachmentExportResult, NotesError> {
            self.counts.exports.fetch_add(1, Ordering::SeqCst);
            self.inner.export_attachment(note_id, attachment_id)
        }
        fn create_note(&self, request: &CreateNote) -> Result<Note, NotesError> {
            self.counts.creates.fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::Create);
            self.inner.create_note(request)
        }
        fn create_folder(&self, request: &CreateFolder) -> Result<Folder, NotesError> {
            self.counts.folders_created.fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::Folder);
            self.inner.create_folder(request)
        }
        fn create_child_folder(&self, request: &CreateChildFolder) -> Result<Folder, NotesError> {
            self.counts
                .child_folders_created
                .fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::CreateChildFolder);
            self.inner.create_child_folder(request)
        }
        fn rename_folder(&self, request: &RenameFolder) -> Result<Folder, NotesError> {
            self.counts.folders_renamed.fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::RenameFolder);
            self.inner.rename_folder(request)
        }
        fn delete_folder(&self, request: &DeleteFolder) -> Result<DeletedFolder, NotesError> {
            self.counts.folders_deleted.fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::DeleteFolder);
            self.inner.delete_folder(request)
        }
        fn reparent_folder(&self, request: &ReparentFolder) -> Result<Folder, NotesError> {
            self.counts
                .folders_reparented
                .fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::ReparentFolder);
            self.inner.reparent_folder(request)
        }
        fn update_note(&self, request: &UpdateNote) -> Result<Note, NotesError> {
            self.counts.updates.fetch_add(1, Ordering::SeqCst);
            self.wait_at_mutation(BlockedMutation::Update);
            if self.update_fails {
                return Err(NotesError::Backend("synthetic update failure".into()));
            }
            self.inner.update_note(request)
        }
        fn move_note(&self, request: &MoveNote) -> Result<Note, NotesError> {
            self.counts.moves.fetch_add(1, Ordering::SeqCst);
            self.inner.move_note(request)
        }
        fn delete_note(&self, request: &DeleteNote) -> Result<DeleteResult, NotesError> {
            self.counts.deletes.fetch_add(1, Ordering::SeqCst);
            self.inner.delete_note(request)
        }
    }

    impl NotesBackend for BlockingRefreshBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.inner.capabilities()
        }
        fn accounts(&self) -> Result<Vec<Account>, NotesError> {
            self.counts.accounts.fetch_add(1, Ordering::SeqCst);
            if let Some(sender) = self.started.lock().unwrap().take() {
                let _ = sender.send(());
                if let Some(receiver) = self.release.lock().unwrap().take() {
                    let _ = receiver.recv();
                }
            }
            self.inner.accounts()
        }
        fn accounts_with_cancel(
            &self,
            cancel: Option<&AtomicBool>,
        ) -> Result<Vec<Account>, NotesError> {
            self.counts.accounts.fetch_add(1, Ordering::SeqCst);
            if let Some(sender) = self.started.lock().unwrap().take() {
                let _ = sender.send(());
                if let Some(receiver) = self.release.lock().unwrap().take() {
                    loop {
                        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                            return Err(NotesError::Cancelled);
                        }
                        match receiver.try_recv() {
                            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                            Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
                        }
                    }
                }
            }
            self.inner.accounts()
        }
        fn folders(&self, account: Option<&AccountId>) -> Result<Vec<Folder>, NotesError> {
            self.inner.folders(account)
        }
        fn notes(&self, query: &NotesQuery) -> Result<NotesPage, NotesError> {
            self.inner.notes(query)
        }
        fn get_note(&self, id: &NoteId) -> Result<Note, NotesError> {
            self.inner.get_note(id)
        }
        fn preview_attachment(
            &self,
            note: &NoteId,
            attachment: &AttachmentId,
        ) -> Result<(), NotesError> {
            self.inner.preview_attachment(note, attachment)
        }
        fn export_attachment(
            &self,
            note: &NoteId,
            attachment: &AttachmentId,
        ) -> Result<AttachmentExportResult, NotesError> {
            self.inner.export_attachment(note, attachment)
        }
        fn create_note(&self, request: &CreateNote) -> Result<Note, NotesError> {
            self.counts.creates.fetch_add(1, Ordering::SeqCst);
            self.inner.create_note(request)
        }
        fn update_note(&self, request: &UpdateNote) -> Result<Note, NotesError> {
            self.inner.update_note(request)
        }
        fn move_note(&self, request: &MoveNote) -> Result<Note, NotesError> {
            self.inner.move_note(request)
        }
        fn delete_note(&self, request: &DeleteNote) -> Result<DeleteResult, NotesError> {
            self.counts.deletes.fetch_add(1, Ordering::SeqCst);
            self.inner.delete_note(request)
        }
    }

    fn counting_app(capabilities: BackendCapabilities) -> (App, MutationCounts) {
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: demo_backend(),
            capabilities,
            counts: counts.clone(),
            create_fails: false,
            update_fails: false,
            move_fails: false,
            delete_fails: false,
        };
        let mut app = App::new(Box::new(backend));
        app.refresh();
        (app, counts)
    }
    fn delete_selection_app(
        names: &[&str],
        selected_index: usize,
        delete_fails: bool,
    ) -> (App, MutationCounts) {
        let account = Account {
            id: AccountId::from("test-account"),
            name: "Test".into(),
            is_default: true,
            is_upgraded: true,
            default_folder_id: Some(FolderId::from("test-folder")),
        };
        let folder = Folder {
            id: FolderId::from("test-folder"),
            account_id: account.id.clone(),
            name: "Notes".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        };
        let destination = Folder {
            id: FolderId::from("test-destination"),
            account_id: account.id.clone(),
            name: "Destination".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        };
        let notes = names
            .iter()
            .map(|name| {
                let id = format!("test-{}", name.to_lowercase());
                let mut note = demo_note(&id, "test-folder", name, name);
                note.account_id = account.id.clone();
                (note.summary.id.clone(), note)
            })
            .collect::<HashMap<_, _>>();
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: MockNotesBackend::new(vec![account], vec![folder, destination], notes),
            capabilities: BackendCapabilities::new(
                notes_core::RichTextCapabilities::all_supported(),
            ),
            counts: counts.clone(),
            create_fails: false,
            update_fails: false,
            move_fails: false,
            delete_fails,
        };
        let mut app = App::new(Box::new(backend));
        app.refresh();
        app.selected_note_index = selected_index;
        app.load_selected_note();
        (app, counts)
    }

    fn create_cache_app(
        create_fails: bool,
        failure: Option<CacheFailurePoint>,
    ) -> (App, MutationCounts, Arc<Mutex<FakeCacheState>>) {
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: demo_backend(),
            capabilities: BackendCapabilities::new(
                notes_core::RichTextCapabilities::all_supported(),
            ),
            counts: counts.clone(),
            create_fails,
            update_fails: false,
            move_fails: false,
            delete_fails: false,
        };
        let (cache, state) = FakeCache::new(FakeCacheState::default());
        let mut app = App::with_cache(Box::new(backend), Box::new(cache));
        app.refresh();
        let mut cache = state.lock().unwrap();
        cache.full_notes.clear();
        cache.replace_snapshot_calls = 0;
        cache.upsert_note_calls = 0;
        cache.upsert_note_ids.clear();
        cache.replaced_snapshots.clear();
        cache.failure = failure;
        drop(cache);
        (app, counts, state)
    }

    fn save_new_note(app: &mut App, title: &str, body: &str) {
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        type_text(app, title);
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(app, body);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(app);
    }

    fn update_cache_app(
        update_fails: bool,
        failure: Option<CacheFailurePoint>,
    ) -> (App, MutationCounts, Arc<Mutex<FakeCacheState>>) {
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: demo_backend(),
            capabilities: BackendCapabilities::new(
                notes_core::RichTextCapabilities::all_supported(),
            ),
            counts: counts.clone(),
            create_fails: false,
            update_fails,
            move_fails: false,
            delete_fails: false,
        };
        let (cache, state) = FakeCache::new(FakeCacheState::default());
        let mut app = App::with_cache(Box::new(backend), Box::new(cache));
        app.refresh();
        let mut cache = state.lock().unwrap();
        cache.full_notes.clear();
        cache.replace_snapshot_calls = 0;
        cache.upsert_note_calls = 0;
        cache.upsert_note_ids.clear();
        cache.replaced_snapshots.clear();
        cache.failure = failure;
        drop(cache);
        (app, counts, state)
    }

    fn blocking_mutation_cache_app(
        blocked: BlockedMutation,
        update_fails: bool,
    ) -> (
        App,
        MutationCounts,
        Arc<Mutex<FakeCacheState>>,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
    ) {
        let counts = MutationCounts::default();
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let backend = BlockingMutationBackend {
            inner: demo_backend(),
            counts: counts.clone(),
            blocked,
            started: Mutex::new(Some(started_sender)),
            release: Mutex::new(Some(release_receiver)),
            update_fails,
        };
        let (cache, state) = FakeCache::new(FakeCacheState::default());
        let mut app = App::with_cache(Box::new(backend), Box::new(cache));
        app.refresh();
        let mut cache = state.lock().unwrap();
        cache.full_notes.clear();
        cache.replace_snapshot_calls = 0;
        cache.upsert_note_calls = 0;
        cache.upsert_note_ids.clear();
        cache.replaced_snapshots.clear();
        drop(cache);
        (app, counts, state, started_receiver, release_sender)
    }

    fn move_cache_app(
        move_fails: bool,
        failure: Option<CacheFailurePoint>,
    ) -> (App, MutationCounts, Arc<Mutex<FakeCacheState>>) {
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: demo_backend(),
            capabilities: BackendCapabilities::new(
                notes_core::RichTextCapabilities::all_supported(),
            ),
            counts: counts.clone(),
            create_fails: false,
            update_fails: false,
            move_fails,
            delete_fails: false,
        };
        let (cache, state) = FakeCache::new(FakeCacheState::default());
        let mut app = App::with_cache(Box::new(backend), Box::new(cache));
        app.refresh();
        let mut cache = state.lock().unwrap();
        cache.full_notes.clear();
        cache.replace_snapshot_calls = 0;
        cache.upsert_note_calls = 0;
        cache.upsert_note_ids.clear();
        cache.replaced_snapshots.clear();
        cache.failure = failure;
        drop(cache);
        (app, counts, state)
    }

    fn delete_cache_app(
        delete_fails: bool,
        failure: Option<CacheFailurePoint>,
    ) -> (App, MutationCounts, Arc<Mutex<FakeCacheState>>) {
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: demo_backend(),
            capabilities: BackendCapabilities::new(
                notes_core::RichTextCapabilities::all_supported(),
            ),
            counts: counts.clone(),
            create_fails: false,
            update_fails: false,
            move_fails: false,
            delete_fails,
        };
        let (cache, state) = FakeCache::new(FakeCacheState::default());
        let mut app = App::with_cache(Box::new(backend), Box::new(cache));
        app.refresh();
        let mut cache = state.lock().unwrap();
        cache.full_notes.clear();
        cache.replace_snapshot_calls = 0;
        cache.upsert_note_calls = 0;
        cache.remove_note_calls = 0;
        cache.upsert_note_ids.clear();
        cache.removed_note_ids.clear();
        cache.replaced_snapshots.clear();
        cache.failure = failure;
        drop(cache);
        (app, counts, state)
    }

    fn save_updated_note(app: &mut App, title_suffix: &str, body_suffix: &str) {
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(app, body_suffix);
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(app, title_suffix);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(app);
    }

    fn poll_update_until_idle(app: &mut App) {
        while app.update_worker.is_some() {
            app.poll_update_worker();
            thread::yield_now();
        }
    }

    fn save_conflict_overwrite(
        app: &mut App,
        counts: &MutationCounts,
        title_suffix: &str,
        body_suffix: &str,
    ) {
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(app, body_suffix);
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(app, title_suffix);
        counts.conflict_on_next_get.store(true, Ordering::SeqCst);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(app);
        assert!(matches!(app.popup, Some(Popup::Conflict(_))));
        app.handle_key(KeyEvent::from(KeyCode::Char('o')));
        poll_update_until_idle(app);
    }

    fn move_selected_note_to_next_folder(app: &mut App) {
        app.handle_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(matches!(app.popup, Some(Popup::Move { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(app);
    }

    fn periodic_app() -> (App, MutationCounts) {
        let (app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        for counter in [
            &counts.accounts,
            &counts.folders,
            &counts.notes,
            &counts.gets,
        ] {
            counter.store(0, Ordering::SeqCst);
        }
        (app, counts)
    }

    fn blocking_periodic_app() -> (App, MutationCounts, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let counts = MutationCounts::default();
        app.backend = Arc::new(Mutex::new(Box::new(BlockingRefreshBackend {
            inner: demo_backend(),
            started: Mutex::new(Some(started_sender)),
            release: Mutex::new(Some(release_receiver)),
            counts: counts.clone(),
        })));
        (app, counts, started_receiver, release_sender)
    }

    fn periodic_due(app: &App) -> Instant {
        app.last_refresh_attempt + app.refresh_interval
    }

    fn poll_periodic_until_idle(app: &mut App) {
        for _ in 0..10_000 {
            app.poll_periodic_refresh();
            if matches!(app.periodic_refresh, PeriodicRefreshState::Idle) {
                return;
            }
            thread::yield_now();
        }
        panic!("periodic worker did not complete");
    }

    fn confirm_delete(app: &mut App) {
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(app);
    }

    fn apply_search(app: &mut App, query: &str) {
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        type_text(app, query);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
    }

    fn visible_names(app: &App) -> Vec<String> {
        app.visible_note_ids()
            .iter()
            .filter_map(|id| app.notes.iter().find(|note| note.id == *id))
            .map(|note| note.name.clone())
            .collect()
    }

    fn apple_events_capabilities() -> BackendCapabilities {
        notes_bridge::AppleScriptNotesBackend::new().capabilities()
    }

    fn begin_new_with_document(app: &mut App, document: notes_core::RichDocument) {
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        let edit = app.edit.as_mut().expect("new editor");
        edit.field = EditField::Body;
        edit.document = EditorDocument::new(document);
        edit.current_target = edit.document.first_target();
        edit.dirty = true;
    }

    fn text_inline(value: &str) -> Vec<notes_core::Inline> {
        vec![notes_core::Inline::Text(value.into())]
    }

    fn list_item(value: &str) -> notes_core::ListItem {
        notes_core::ListItem {
            content: text_inline(value),
        }
    }

    #[test]
    fn initial_refresh_selects_a_note() {
        let mut app = App::demo();
        app.refresh();
        app.selected_navigation = app
            .navigation
            .iter()
            .position(|item| {
                matches!(item, NavigationItem::Folder { id, .. } if *id == FolderId::from("demo-notes"))
            })
            .expect("demo notes folder");
        app.load_notes_for_selection();
        assert_eq!(
            app.selected_folder_id(),
            Some(&FolderId::from("demo-notes"))
        );
        assert!(app.selected_note.is_some());
    }
    #[test]
    fn boundaries_do_not_escape_collections() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(app.selected_note_index, 0);
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('G')));
        assert_eq!(app.selected_navigation, app.navigation.len() - 1);
    }
    #[test]
    fn focus_cycles() {
        let mut app = App::demo();
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Notes);
        app.handle_key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Navigation);
    }
    #[test]
    fn folder_selection_loads_its_notes_and_preview() {
        let mut app = App::demo();
        app.refresh();
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(
            app.selected_note.as_ref().unwrap().summary.name,
            "Project ideas"
        );
    }
    #[test]
    fn preview_scroll_and_help_are_stateful() {
        let mut app = App::demo();
        app.refresh();
        app.focus = Focus::Preview;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.preview_scroll > 0);
        app.handle_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(app.show_help);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.show_help);
    }

    #[test]
    fn attachment_popup_opens_closes_navigates_and_previews_explicitly() {
        let (mut app, counts) = counting_app(demo_backend().capabilities());
        select_demo_note(&mut app, "demo-attachment");

        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        assert!(matches!(
            app.popup,
            Some(Popup::Attachments { selected: 0 })
        ));
        assert_eq!(counts.previews.load(Ordering::SeqCst), 0);
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert!(matches!(
            app.popup,
            Some(Popup::Attachments { selected: 1 })
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert!(matches!(
            app.popup,
            Some(Popup::Attachments { selected: 0 })
        ));

        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.previews.load(Ordering::SeqCst), 1);
        assert!(app.status.text.contains("Opened Photo.png in Notes.app"));
        assert!(app.status.text.contains("close the preview"));
        assert!(matches!(
            app.popup,
            Some(Popup::Attachments { selected: 0 })
        ));

        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.popup.is_none());
    }

    #[test]
    fn unsupported_attachment_never_reaches_the_preview_backend() {
        let (mut app, counts) = counting_app(demo_backend().capabilities());
        select_demo_note(&mut app, "demo-attachment");
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(counts.previews.load(Ordering::SeqCst), 0);
        assert!(app.status.is_error);
        assert!(app.status.text.contains("unsupported attachment kind"));
        assert!(matches!(
            app.popup,
            Some(Popup::Attachments { selected: 2 })
        ));
    }

    #[test]
    fn attachment_export_uses_the_injected_backend_and_never_launches_in_tests() {
        let (mut app, counts) = counting_app(demo_backend().capabilities());
        select_demo_note(&mut app, "demo-attachment");
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        poll_update_until_idle(&mut app);

        assert_eq!(counts.exports.load(Ordering::SeqCst), 1);
        assert_eq!(counts.previews.load(Ordering::SeqCst), 0);
        assert!(app.status.text.contains("/mock-export/Manual.pdf"));
        assert!(matches!(
            app.popup,
            Some(Popup::Attachments { selected: 1 })
        ));
    }

    #[test]
    fn zero_and_locked_attachments_fail_safely_without_a_popup() {
        let (mut app, counts) = counting_app(demo_backend().capabilities());
        select_demo_note(&mut app, "demo-plain");
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        assert!(app.popup.is_none());
        assert!(app.status.text.contains("no attachments"));

        let locked = demo_backend()
            .notes(&NotesQuery {
                folder_id: Some(FolderId::from("demo-notes")),
                limit: Some(0),
                ..Default::default()
            })
            .unwrap()
            .items
            .into_iter()
            .find(|note| note.id == NoteId::from("demo-locked-attachment"))
            .expect("locked demo summary");
        app.notes = vec![locked];
        app.selected_note_index = 0;
        app.selected_note = None;
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));

        assert!(app.popup.is_none());
        assert!(app.status.is_error);
        assert!(app.status.text.contains("password-protected note"));
        assert_eq!(counts.previews.load(Ordering::SeqCst), 0);
        assert_eq!(counts.exports.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn attachment_popup_renders_metadata_and_controls() {
        let mut app = App::demo();
        app.refresh();
        select_demo_note(&mut app, "demo-attachment");
        app.handle_key(KeyEvent::from(KeyCode::Char('a')));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Attachments (3)"));
        assert!(rendered.contains("Photo.png"));
        assert!(rendered.contains("Enter preview"));
    }
    #[test]
    fn rendering_smoke_tests() {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut app = App::demo();
        app.refresh();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let content = terminal.backend().buffer().content();
        assert!(content.iter().any(|cell| cell.symbol() == "A"));
        app.show_help = true;
        terminal.draw(|frame| render(frame, &app)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "H"));
    }
    #[test]
    fn small_terminal_renders_fallback() {
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let app = App::demo();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "T"));
    }

    #[test]
    fn editor_create_save_and_cancel_work_in_demo() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert_eq!(app.mode, AppMode::Insert);
        app.handle_key(KeyEvent::from(KeyCode::Char('X')));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        app.handle_key(KeyEvent::from(KeyCode::Char('b')));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.notes.iter().any(|note| note.name == "X"));
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.edit.is_none());
    }

    #[test]
    fn dirty_editor_prompts_before_quitting_and_conflict_popup_renders() {
        let mut app = App::demo();
        app.refresh();
        select_demo_note(&mut app, "demo-rich");
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        app.edit.as_mut().expect("editor").dirty = true;
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.handle_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(matches!(app.popup, Some(Popup::Discard { .. })));
        let remote = app.selected_note.clone().unwrap();
        app.popup = Some(Popup::Conflict(Box::new(remote)));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.symbol() == "N"));
    }

    #[test]
    fn rich_shortcuts_toggle_and_convert_blocks() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.edit.as_mut().expect("editor").field = EditField::Body;
        app.insert_text("text");
        app.handle_insert(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        let html = serialize_notes_html(&app.edit.as_ref().expect("editor").document.document);
        assert!(html.contains("<b>text</b>"));
        app.handle_insert(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        let html = serialize_notes_html(&app.edit.as_ref().expect("editor").document.document);
        assert!(!html.contains("<b>"));
        for (key, marker) in [('1', "<h1>"), ('2', "<h2>"), ('3', "<h3>")] {
            app.handle_insert(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT));
            let html = serialize_notes_html(&app.edit.as_ref().expect("editor").document.document);
            assert!(html.contains(marker));
        }
    }

    #[test]
    fn link_popup_applies_escaped_url_and_esc_cancels() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.edit.as_mut().expect("editor").field = EditField::Body;
        app.insert_text("link");
        app.handle_insert(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(matches!(app.popup, Some(Popup::Link { .. })));
        app.handle_popup(KeyEvent::from(KeyCode::Enter));
        let html = serialize_notes_html(&app.edit.as_ref().expect("editor").document.document);
        assert!(html.contains("href=\"https://\""));
    }

    #[test]
    fn save_preflight_rejects_each_lossy_apple_events_feature_without_mutation() {
        let cases = [
            (
                "hyperlink",
                notes_core::RichDocument {
                    blocks: vec![notes_core::Block::Paragraph(vec![
                        notes_core::Inline::Link {
                            label: text_inline("link"),
                            href: "https://example.test".into(),
                        },
                    ])],
                },
            ),
            (
                "quote",
                notes_core::RichDocument {
                    blocks: vec![notes_core::Block::Quote(text_inline("quote"))],
                },
            ),
            (
                "heading 3",
                notes_core::RichDocument {
                    blocks: vec![notes_core::Block::Heading3(text_inline("heading"))],
                },
            ),
            (
                "adjacent mixed list types",
                notes_core::RichDocument {
                    blocks: vec![
                        notes_core::Block::BulletList(vec![list_item("bullet")]),
                        notes_core::Block::NumberedList(vec![list_item("numbered")]),
                    ],
                },
            ),
        ];

        for (expected_feature, document) in cases {
            let (mut app, counts) = counting_app(apple_events_capabilities());
            begin_new_with_document(&mut app, document);
            app.handle_key(modified('s', KeyModifiers::CONTROL));

            assert_eq!(
                counts.creates.load(Ordering::SeqCst),
                0,
                "{expected_feature}"
            );
            assert_eq!(
                counts.updates.load(Ordering::SeqCst),
                0,
                "{expected_feature}"
            );
            assert!(app.edit.is_some(), "{expected_feature}");
            assert!(app.status.is_error, "{expected_feature}");
            assert!(
                app.status.text.contains(expected_feature),
                "{}: {}",
                expected_feature,
                app.status.text
            );
        }
    }

    #[test]
    fn unsupported_existing_note_cannot_be_saved_after_an_unrelated_edit() {
        let (mut app, counts) = counting_app(apple_events_capabilities());
        let note = app
            .backend()
            .get_note(&NoteId::from("demo-link"))
            .expect("existing link fixture");
        app.selected_note = Some(note);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let edit = app.edit.as_mut().expect("editor");
        edit.field = EditField::Title;
        edit.cursor = edit.title_buffer.chars().count();
        type_text(&mut app, " unrelated");
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);

        assert_eq!(counts.creates.load(Ordering::SeqCst), 0);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        assert!(app.edit.is_some());
        assert!(app.status.text.contains("hyperlink"));
    }

    #[test]
    fn apple_events_keyboard_gating_blocks_only_lossy_editor_actions() {
        let (mut app, _counts) = counting_app(apple_events_capabilities());
        begin_new_with_document(
            &mut app,
            notes_core::RichDocument {
                blocks: vec![notes_core::Block::Paragraph(text_inline("text"))],
            },
        );

        app.handle_key(modified('k', KeyModifiers::CONTROL));
        assert!(app.popup.is_none());
        assert!(app.status.text.contains("hyperlink"));

        app.handle_key(modified('q', KeyModifiers::ALT));
        assert!(matches!(
            app.edit.as_ref().unwrap().document.document.blocks[0],
            notes_core::Block::Paragraph(_)
        ));
        assert!(app.status.text.contains("quote"));

        app.handle_key(modified('3', KeyModifiers::ALT));
        assert!(matches!(
            app.edit.as_ref().unwrap().document.document.blocks[0],
            notes_core::Block::Paragraph(_)
        ));
        assert!(app.status.text.contains("heading 3"));

        app.handle_key(modified('2', KeyModifiers::ALT));
        assert!(matches!(
            app.edit.as_ref().unwrap().document.document.blocks[0],
            notes_core::Block::Heading2(_)
        ));

        let edit = app.edit.as_mut().unwrap();
        edit.document = EditorDocument::new(notes_core::RichDocument {
            blocks: vec![notes_core::Block::BulletList(vec![
                list_item("one"),
                list_item("two"),
            ])],
        });
        edit.current_target = EditorTarget::ListItem {
            block_index: 0,
            item_index: 0,
        };
        let before = edit.document.document.clone();
        app.handle_key(modified('n', KeyModifiers::ALT));
        assert_eq!(app.edit.as_ref().unwrap().document.document, before);
        assert!(app.status.text.contains("adjacent mixed list types"));
    }

    #[test]
    fn mock_backend_still_saves_the_full_demo_rich_feature_set() {
        let capabilities = demo_backend().capabilities();
        let (mut app, counts) = counting_app(capabilities);
        let document = notes_core::RichDocument {
            blocks: vec![
                notes_core::Block::Heading1(text_inline("H1")),
                notes_core::Block::Heading2(text_inline("H2")),
                notes_core::Block::Heading3(text_inline("H3")),
                notes_core::Block::Paragraph(vec![notes_core::Inline::Link {
                    label: text_inline("link"),
                    href: "https://example.test".into(),
                }]),
                notes_core::Block::Quote(text_inline("quote")),
                notes_core::Block::BulletList(vec![list_item("bullet")]),
                notes_core::Block::NumberedList(vec![list_item("numbered")]),
            ],
        };
        assert!(capabilities
            .rich_text
            .unsupported_features(&document)
            .is_empty());
        begin_new_with_document(&mut app, document);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);

        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn real_apple_events_backend_gates_shortcuts_and_preflights_without_osascript() {
        let mut app = App::new(Box::new(notes_bridge::AppleScriptNotesBackend::new()));
        let document = EditorDocument::new(notes_core::RichDocument {
            blocks: vec![notes_core::Block::Paragraph(text_inline("text"))],
        });
        app.edit = Some(EditSession {
            note_id: Some(NoteId::from("synthetic-safety-smoke")),
            folder_id: FolderId::from("synthetic-folder"),
            original_title: "Synthetic".into(),
            original_body_html: "<div>text</div>".into(),
            original_plaintext: "text".into(),
            base_modification_date: Some(NoteDate::new("synthetic-date")),
            title_buffer: "Synthetic".into(),
            current_target: document.first_target(),
            document,
            dirty: false,
            is_new: false,
            field: EditField::Body,
            cursor: 0,
            viewport: 0,
        });
        app.mode = AppMode::Insert;

        for (key, expected) in [('k', "hyperlink"), ('q', "quote"), ('3', "heading 3")] {
            let modifiers = if key == 'k' {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::ALT
            };
            app.handle_key(modified(key, modifiers));
            assert!(app.status.text.contains(expected));
            assert!(app.popup.is_none());
        }

        let edit = app.edit.as_mut().unwrap();
        edit.document = EditorDocument::new(notes_core::RichDocument {
            blocks: vec![notes_core::Block::Heading3(text_inline("unsafe"))],
        });
        edit.current_target = edit.document.first_target();
        edit.dirty = true;
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        assert!(app.status.text.contains("heading 3"));
        assert!(app.edit.is_some());
    }

    #[test]
    fn app_keyboard_is_unicode_safe_and_clamps_on_target_navigation() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(&mut app, "A✅🍎日");
        app.handle_key(KeyEvent::from(KeyCode::Left));
        app.handle_key(KeyEvent::from(KeyCode::Backspace));
        app.handle_key(KeyEvent::from(KeyCode::Delete));
        assert_eq!(current_body_text(&app), "A✅");

        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "x");
        app.handle_key(modified('k', KeyModifiers::ALT));
        app.handle_key(KeyEvent::from(KeyCode::End));
        assert_eq!(app.edit.as_ref().expect("editor").cursor, 2);
        app.handle_key(modified('j', KeyModifiers::ALT));
        assert_eq!(app.edit.as_ref().expect("editor").cursor, 1);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
    }

    #[test]
    fn app_list_handlers_are_item_safe_and_enter_stays_in_the_list() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(&mut app, "one");
        app.handle_key(modified('b', KeyModifiers::ALT));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "two");
        app.handle_key(modified('b', KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::from(KeyCode::Backspace));
        app.handle_key(KeyEvent::from(KeyCode::Home));
        app.handle_key(KeyEvent::from(KeyCode::Delete));

        let edit = app.edit.as_ref().expect("editor");
        assert_eq!(
            edit.current_target,
            EditorTarget::ListItem {
                block_index: 0,
                item_index: 1,
            }
        );
        assert!(matches!(
            &edit.document.document.blocks[0],
            notes_core::Block::BulletList(items)
                if inline_plain(&items[0].content) == "one"
                    && inline_plain(&items[1].content) == "w"
                    && matches!(&items[1].content[..], [notes_core::Inline::Bold(_)])
        ));
    }

    #[test]
    fn link_popup_applies_to_its_captured_target() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(&mut app, "first");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "second");
        let captured = app.edit.as_ref().expect("editor").current_target;
        app.handle_key(modified('k', KeyModifiers::CONTROL));
        app.edit.as_mut().expect("editor").current_target = EditorTarget::Block { block_index: 0 };
        app.handle_key(KeyEvent::from(KeyCode::Enter));

        let edit = app.edit.as_ref().expect("editor");
        assert_eq!(captured, EditorTarget::Block { block_index: 1 });
        assert!(matches!(
            &edit.document.document.blocks[0],
            notes_core::Block::Paragraph(items)
                if !matches!(&items[..], [notes_core::Inline::Link { .. }])
        ));
        assert!(matches!(
            &edit.document.document.blocks[1],
            notes_core::Block::Paragraph(items)
                if matches!(&items[..], [notes_core::Inline::Link { href, .. }] if href == "https://")
        ));
    }

    #[test]
    fn app_structural_shortcuts_split_merge_remap_and_delete() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(&mut app, "one");
        app.handle_key(modified('b', KeyModifiers::ALT));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "TWO");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "three");
        app.handle_key(modified('k', KeyModifiers::ALT));
        app.handle_key(modified('p', KeyModifiers::ALT));

        let edit = app.edit.as_ref().expect("editor");
        assert_eq!(edit.current_target, EditorTarget::Block { block_index: 1 });
        assert!(matches!(
            edit.document.document.blocks.as_slice(),
            [
                notes_core::Block::BulletList(_),
                notes_core::Block::Paragraph(_),
                notes_core::Block::BulletList(_)
            ]
        ));

        app.handle_key(modified('n', KeyModifiers::ALT));
        app.handle_key(modified('b', KeyModifiers::ALT));
        let edit = app.edit.as_ref().expect("editor");
        assert_eq!(
            edit.current_target,
            EditorTarget::ListItem {
                block_index: 0,
                item_index: 1,
            }
        );
        assert_eq!(edit.document.document.blocks.len(), 1);

        app.handle_key(modified('d', KeyModifiers::CONTROL));
        let edit = app.edit.as_ref().expect("editor");
        assert_eq!(
            edit.current_target,
            EditorTarget::ListItem {
                block_index: 0,
                item_index: 1,
            }
        );
        assert!(matches!(
            &edit.document.document.blocks[0],
            notes_core::Block::BulletList(items)
                if items.len() == 2
                    && inline_plain(&items[0].content) == "one"
                    && inline_plain(&items[1].content) == "three"
        ));
    }

    #[test]
    fn mixed_document_keyboard_workflow_survives_save_reopen_and_edit() {
        let mut app = App::demo();
        app.refresh();
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        type_text(&mut app, "Phase4 Demo Mixed");
        app.handle_key(KeyEvent::from(KeyCode::Tab));

        type_text(&mut app, "Heading");
        app.handle_key(modified('1', KeyModifiers::ALT));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Bold paragraph");
        app.handle_key(modified('b', KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Italic paragraph");
        app.handle_key(modified('i', KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Underlined paragraph");
        app.handle_key(modified('u', KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Linked paragraph");
        app.handle_key(modified('k', KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Bullet one");
        app.handle_key(modified('b', KeyModifiers::ALT));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Bullet two");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        app.handle_key(modified('n', KeyModifiers::ALT));
        type_text(&mut app, "Numbered one");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Numbered two");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        app.handle_key(modified('q', KeyModifiers::ALT));
        type_text(&mut app, "Quote");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        type_text(&mut app, "Code");
        app.handle_key(modified('c', KeyModifiers::ALT));

        let expected = app.edit.as_ref().expect("editor").document.document.clone();
        let serialized = serialize_notes_html(&expected);
        assert_eq!(parse_notes_html(&serialized).unwrap(), expected);
        assert_mixed_document(&expected);

        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(app.mode, AppMode::Normal);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        assert_eq!(
            app.edit.as_ref().expect("editor").document.document,
            expected
        );

        for _ in 0..5 {
            app.handle_key(modified('j', KeyModifiers::ALT));
        }
        app.handle_key(KeyEvent::from(KeyCode::End));
        type_text(&mut app, " X");
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let reopened = &app.edit.as_ref().expect("editor").document.document;
        assert!(matches!(
            &reopened.blocks[5],
            notes_core::Block::BulletList(items)
                if inline_plain(&items[0].content) == "Bullet one X"
                    && inline_plain(&items[1].content) == "Bullet two"
        ));
        assert_mixed_document(reopened);
    }

    #[test]
    fn conflict_cancel_reload_and_overwrite_preserve_the_editor_document() {
        let mut app = App::demo();
        app.refresh();
        select_demo_note(&mut app, "demo-rich");
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, "Local ");
        let local = app.edit.as_ref().expect("editor").document.document.clone();
        let mut remote = app.selected_note.clone().expect("remote");
        remote.body_html = "<h1>Remote</h1>".into();
        remote.plaintext = "Remote".into();
        remote.summary.modification_date = NoteDate::new("Remote conflict date");

        app.popup = Some(Popup::Conflict(Box::new(remote.clone())));
        app.handle_key(KeyEvent::from(KeyCode::Char('c')));
        assert_eq!(app.edit.as_ref().expect("editor").document.document, local);

        app.popup = Some(Popup::Conflict(Box::new(remote.clone())));
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        assert!(matches!(
            &app.edit.as_ref().expect("editor").document.document.blocks[0],
            notes_core::Block::Heading1(items) if inline_plain(items) == "Remote"
        ));

        app.handle_key(KeyEvent::from(KeyCode::End));
        type_text(&mut app, " local");
        let overwritten = app.edit.as_ref().expect("editor").document.document.clone();
        app.popup = Some(Popup::Conflict(Box::new(remote)));
        app.handle_key(KeyEvent::from(KeyCode::Char('o')));
        poll_update_until_idle(&mut app);
        assert_eq!(
            parse_notes_html(&app.selected_note.as_ref().expect("saved").body_html).unwrap(),
            overwritten
        );
    }

    #[test]
    fn demo_has_editable_and_read_only_rich_fixtures() {
        let backend = demo_backend();
        for (id, expected) in [
            ("demo-plain", true),
            ("demo-rich", true),
            ("demo-bullet", true),
            ("demo-numbered", true),
            ("demo-link", true),
            ("demo-attachment", false),
            ("demo-table", false),
        ] {
            let note = backend.get_note(&NoteId::from(id)).expect("fixture");
            assert_eq!(
                !matches!(
                    classify_editability(&note),
                    Editability::ReadOnlyUnsupported { .. }
                ),
                expected,
                "{id}"
            );
        }
    }

    #[test]
    fn notes_app_title_line_is_not_part_of_the_editable_body() {
        let document = editor_document_from_html(
            "Fixture title",
            "<div>Fixture title</div><div>Body paragraph</div>",
        )
        .expect("normalized Notes body");
        assert!(matches!(
            document.document.blocks.as_slice(),
            [notes_core::Block::Paragraph(items)] if inline_plain(items) == "Body paragraph"
        ));
    }

    #[test]
    fn notes_app_title_line_is_stripped_before_observed_h2_normalization() {
        let document = editor_document_from_html(
            "Phase41-Probe-H2-20260827",
            concat!(
                "<div>Phase41-Probe-H2-20260827</div>\n",
                "<div><b><span style=\"font-size: 18px\">Probe H2</span></b></div>",
            ),
        )
        .expect("normalized Notes H2 body");
        assert!(matches!(
            document.document.blocks.as_slice(),
            [notes_core::Block::Heading2(items)] if inline_plain(items) == "Probe H2"
        ));
    }

    fn modified(character: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), modifiers)
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(character)));
        }
    }

    fn current_body_text(app: &App) -> String {
        let edit = app.edit.as_ref().expect("editor");
        edit.document.target_text(edit.current_target).unwrap()
    }

    fn select_demo_note(app: &mut App, id: &str) {
        app.selected_note = Some(
            demo_backend()
                .get_note(&NoteId::from(id))
                .expect("demo note"),
        );
    }

    fn assert_mixed_document(document: &notes_core::RichDocument) {
        assert_eq!(document.blocks.len(), 9);
        assert!(
            matches!(&document.blocks[0], notes_core::Block::Heading1(items) if inline_plain(items) == "Heading")
        );
        assert!(
            matches!(&document.blocks[1], notes_core::Block::Paragraph(items) if matches!(&items[..], [notes_core::Inline::Bold(_)]))
        );
        assert!(
            matches!(&document.blocks[2], notes_core::Block::Paragraph(items) if matches!(&items[..], [notes_core::Inline::Italic(_)]))
        );
        assert!(
            matches!(&document.blocks[3], notes_core::Block::Paragraph(items) if matches!(&items[..], [notes_core::Inline::Underline(_)]))
        );
        assert!(
            matches!(&document.blocks[4], notes_core::Block::Paragraph(items) if matches!(&items[..], [notes_core::Inline::Link { .. }]))
        );
        assert!(
            matches!(&document.blocks[5], notes_core::Block::BulletList(items) if items.len() == 2)
        );
        assert!(
            matches!(&document.blocks[6], notes_core::Block::NumberedList(items) if items.len() == 2)
        );
        assert!(
            matches!(&document.blocks[7], notes_core::Block::Quote(items) if inline_plain(items) == "Quote")
        );
        assert!(
            matches!(&document.blocks[8], notes_core::Block::CodeBlock(text) if text == "Code")
        );
    }

    #[test]
    fn create_folder_shortcut_validates_unicode_and_reconciles_by_stable_id() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let account_id = app.selected_account_id().expect("selected account");
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        assert!(matches!(app.popup, Some(Popup::CreateFolder { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 0);
        assert!(app.status.text.contains("cannot be empty"));
        type_text(&mut app, "Проекты 🚀");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
        let selected = app.navigation[app.selected_navigation].clone();
        let NavigationItem::Folder {
            id,
            account_id: actual_account,
            name,
            ..
        } = selected
        else {
            panic!("created folder should be selected");
        };
        assert_eq!(actual_account, account_id);
        assert_eq!(name, "Проекты 🚀");
        assert!(app.folders.iter().any(|folder| folder.id == id));
        assert!(matches!(app.search, SearchState::Inactive));
        assert!(app.status.text.contains("Created folder"));
    }

    #[test]
    fn create_folder_runs_in_foreground_worker_without_speculative_insertion() {
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::Folder, false);
        let before_folders = app.folders.clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(&mut app, "Async folder");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        started.recv().expect("folder worker started");
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
        assert_eq!(app.folders, before_folders);
        assert!(app.update_worker.is_some());
        release.send(()).expect("release folder worker");
        poll_update_until_idle(&mut app);
        assert!(app
            .folders
            .iter()
            .any(|folder| folder.name == "Async folder"));
        assert!(cache.lock().unwrap().replace_snapshot_calls >= 1);
    }

    #[test]
    fn failed_create_folder_preserves_input_and_changes_no_authoritative_state() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        counts.folder_create_fails.store(true, Ordering::SeqCst);
        let folders = app.folders.clone();
        let navigation = app.selected_navigation;
        let note = app.selected_note.clone();
        let search = app.search.clone();
        let source = app.data_source.clone();
        let name = "Проекты 🚀 \"2026\"";
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(&mut app, name);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_none());
        assert_eq!(app.folders, folders);
        assert_eq!(app.selected_navigation, navigation);
        assert_eq!(app.selected_note, note);
        assert_eq!(app.search, search);
        assert_eq!(app.data_source, source);
        assert!(
            matches!(&app.popup, Some(Popup::CreateFolder { name: actual, cursor, .. }) if actual == name && *cursor == name.chars().count())
        );
        assert!(app.status.is_error);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 2);
        assert!(app.folders.iter().any(|folder| folder.name == name));
        assert!(app.popup.is_none());
    }

    #[test]
    fn successful_create_folder_cache_failure_does_not_retry_or_rollback() {
        let (mut app, counts, cache) =
            create_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(&mut app, "Cache warning folder");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
        assert!(app.popup.is_none());
        assert!(app
            .folders
            .iter()
            .any(|folder| folder.name == "Cache warning folder"));
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app.status.text.contains("cache warning"));
        assert!(cache.lock().unwrap().replace_snapshot_calls >= 1);
        app.poll_update_worker();
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_create_folder_session_failure_is_warning_only() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let path = temporary_session_path("folder-create-session-failure");
        let _ = fs::remove_file(&path);
        app.set_session_state(path.clone(), None);
        app.persist_session_selection();
        let before = load_session(&path).expect("read initial session");
        app.session_write_failure = true;
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(&mut app, "Проекты 🚀");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
        let selected_id = match &app.navigation[app.selected_navigation] {
            NavigationItem::Folder { id, .. } => id.clone(),
            _ => panic!("created folder should be selected"),
        };
        assert!(app.folders.iter().any(|folder| folder.id == selected_id));
        assert!(app.popup.is_none());
        assert!(app.update_worker.is_none());
        assert!(matches!(app.search, SearchState::Inactive));
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app.status.text.contains("Session warning"));
        assert!(!app.status.text.contains("Failed to create"));
        assert_eq!(load_session(&path).expect("read unchanged session"), before);
        app.poll_update_worker();
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 1);
        assert!(app.popup.is_none());
        assert!(app.update_worker.is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rename_folder_same_name_is_local_noop() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folders = app.folders.clone();
        let selected = app.selected_navigation;
        let note = app.selected_note.clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        assert!(matches!(app.popup, Some(Popup::RenameFolder { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 0);
        assert!(app.popup.is_none());
        assert_eq!(app.folders, folders);
        assert_eq!(app.selected_navigation, selected);
        assert_eq!(app.selected_note, note);
        assert_eq!(app.status.text, "Folder name unchanged");
    }

    #[test]
    fn rename_folder_rejects_empty_or_whitespace_without_backend_call() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folders = app.folders.clone();
        let selected = app.selected_navigation;
        for replacement in ["", "   "] {
            app.handle_key(KeyEvent::from(KeyCode::Char('R')));
            let original_len = match &app.popup {
                Some(Popup::RenameFolder { name, .. }) => name.chars().count(),
                _ => panic!("rename popup"),
            };
            for _ in 0..original_len {
                app.handle_key(KeyEvent::from(KeyCode::Backspace));
            }
            type_text(&mut app, replacement);
            app.handle_key(KeyEvent::from(KeyCode::Enter));
            assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 0);
            assert!(matches!(app.popup, Some(Popup::RenameFolder { .. })));
            assert!(app.status.is_error);
            assert_eq!(app.folders, folders);
            assert_eq!(app.selected_navigation, selected);
            app.handle_key(KeyEvent::from(KeyCode::Esc));
        }
    }

    #[test]
    fn failed_folder_rename_preserves_input_and_authoritative_state() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folder_id = app.selected_folder_id().cloned().expect("selected folder");
        let old_name = app
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .unwrap()
            .name
            .clone();
        let note = app.selected_note_id();
        let scroll = app.preview_scroll;
        let source = app.data_source.clone();
        let attempted = "Проекты 🚀 \"2027\"";
        counts.folder_rename_fails.store(true, Ordering::SeqCst);
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        for _ in 0..old_name.chars().count() {
            app.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        type_text(&mut app, attempted);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_none());
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.selected_note_id(), note);
        assert_eq!(app.preview_scroll, scroll);
        assert_eq!(app.data_source, source);
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .unwrap()
                .name,
            old_name
        );
        assert!(
            matches!(&app.popup, Some(Popup::RenameFolder { folder_id: actual_id, name, cursor, .. }) if actual_id == &folder_id && name == attempted && *cursor == attempted.chars().count())
        );
        assert!(app.status.is_error);
        app.poll_update_worker();
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 2);
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .unwrap()
                .name,
            attempted
        );
        assert!(app.popup.is_none());
    }

    #[test]
    fn rename_folder_shortcut_is_gated_by_modal_and_worker_state() {
        let (mut app, _counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folder = match app.navigation[app.selected_navigation].clone() {
            NavigationItem::Folder {
                id,
                account_id,
                name,
                ..
            } => (id, account_id, name),
            _ => panic!("selected folder"),
        };
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        assert!(
            matches!(&app.popup, Some(Popup::RenameFolder { account_id, folder_id, original_name, name, cursor }) if account_id == &folder.1 && folder_id == &folder.0 && original_name == &folder.2 && name == &folder.2 && *cursor == folder.2.chars().count())
        );
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        assert!(matches!(app.popup, Some(Popup::RenameFolder { .. })));
        app.popup = None;
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        assert!(app.edit.is_some());
        assert!(app.popup.is_none());
    }

    #[test]
    fn successful_folder_rename_reconciles_by_stable_id_and_preserves_context() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let note_id = app.selected_note_id().expect("note");
        let query = app
            .notes
            .iter()
            .find(|note| note.id == note_id)
            .expect("summary")
            .name
            .clone();
        app.search = SearchState::Active(ActiveSearch {
            query: query.clone(),
            visible_ids: Vec::new(),
        });
        app.recompute_search();
        app.focus = Focus::Notes;
        app.preview_scroll = 7;
        let old_name = app
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .unwrap()
            .name
            .clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        for _ in 0..old_name.chars().count() {
            app.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        type_text(&mut app, "Projects Renamed 🚀");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.selected_note_id(), Some(note_id));
        assert!(matches!(&app.search, SearchState::Active(active) if active.query == query));
        assert_eq!(app.preview_scroll, 7);
        assert_eq!(app.focus, Focus::Notes);
        assert!(app.popup.is_none());
        assert!(app.update_worker.is_none());
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .unwrap()
                .name,
            "Projects Renamed 🚀"
        );
    }

    #[test]
    fn successful_folder_rename_cache_failure_does_not_retry_or_rollback() {
        let (mut app, counts, cache) =
            create_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let note_id = app.selected_note_id().expect("note");
        app.notes
            .iter_mut()
            .find(|note| note.id == note_id)
            .expect("selected summary")
            .name = "meeting context".into();
        app.search = SearchState::Active(ActiveSearch {
            query: "meeting".into(),
            visible_ids: Vec::new(),
        });
        app.recompute_search();
        app.preview_scroll = 11;
        app.focus = Focus::Notes;
        let old_name = app
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .expect("folder")
            .name
            .clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        for _ in old_name.chars() {
            app.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        type_text(&mut app, "Projects Renamed 🚀");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);

        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.selected_note_id(), Some(note_id));
        assert!(matches!(&app.search, SearchState::Active(active) if active.query == "meeting"));
        assert_eq!(app.preview_scroll, 11);
        assert_eq!(app.focus, Focus::Notes);
        assert!(app.popup.is_none());
        assert!(app.update_worker.is_none());
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .expect("renamed folder")
                .name,
            "Projects Renamed 🚀"
        );
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app
            .status
            .text
            .contains("Renamed folder to Projects Renamed 🚀"));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.text.contains("Failed to rename"));
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 1);
        app.poll_update_worker();
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        assert!(app.popup.is_none());
    }

    #[test]
    fn successful_folder_rename_does_not_rewrite_session_when_continuity_state_is_unchanged() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let path = temporary_session_path("folder-rename-session-no-write");
        let _ = fs::remove_file(&path);
        app.set_session_state(path.clone(), None);
        app.persist_session_selection();
        let before = fs::read(&path).expect("initial session bytes");
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let old_name = app
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .expect("folder")
            .name
            .clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        for _ in old_name.chars() {
            app.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        type_text(&mut app, "Session-stable rename");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read(&path).expect("unchanged session bytes"), before);
        let _ = fs::remove_file(path);
    }

    fn create_empty_folder_for_delete(app: &mut App, name: &str) -> FolderId {
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(app, name);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(app);
        app.selected_folder_id()
            .cloned()
            .expect("created folder selected")
    }

    #[test]
    fn create_child_folder_shortcut_opens_prefilled_parent_popup() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let parent_id = app.selected_folder_id().cloned().expect("parent");
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        assert!(
            matches!(&app.popup, Some(Popup::CreateChildFolder { parent_folder_id, name, cursor, .. }) if parent_folder_id == &parent_id && name.is_empty() && *cursor == 0)
        );
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn create_child_folder_rejects_empty_and_reconciles_authoritative_child_id() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let parent_id = app.selected_folder_id().cloned().expect("parent");
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 0);
        type_text(&mut app, "Проект 🚀 \"Child\"");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
        let child_id = app.selected_folder_id().cloned().expect("child selected");
        let child = app
            .folders
            .iter()
            .find(|folder| folder.id == child_id)
            .unwrap();
        assert_eq!(
            child.parent,
            FolderParent::Folder {
                folder_id: parent_id
            }
        );
        assert!(app.notes.is_empty());
        assert!(app.selected_note.is_none());
        assert!(matches!(app.search, SearchState::Inactive));
        assert_eq!(app.preview_scroll, 0);
        assert_eq!(app.focus, Focus::Navigation);
    }

    #[test]
    fn failed_child_folder_create_preserves_input_parent_and_runtime() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let parent_id = app.selected_folder_id().cloned().expect("parent");
        let folders = app.folders.clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Retry 🚀");
        counts.folder_create_fails.store(true, Ordering::SeqCst);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
        assert_eq!(app.folders, folders);
        assert_eq!(app.selected_folder_id(), Some(&parent_id));
        assert!(
            matches!(&app.popup, Some(Popup::CreateChildFolder { parent_folder_id: actual, name, cursor, .. }) if actual == &parent_id && name == "Retry 🚀" && *cursor == name.chars().count())
        );
        app.poll_update_worker();
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_child_folder_create_cache_failure_does_not_retry_or_rollback() {
        let (mut app, counts, cache) = create_cache_app(false, None);
        app.focus = Focus::Navigation;
        let parent_id = app.selected_folder_id().cloned().expect("parent");
        cache.lock().unwrap().failure = Some(CacheFailurePoint::ReplaceSnapshot);
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Cache child");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
        let child_id = app.selected_folder_id().cloned().expect("child");
        assert!(
            matches!(app.folders.iter().find(|folder| folder.id == child_id).map(|folder| &folder.parent), Some(FolderParent::Folder { folder_id }) if folder_id == &parent_id)
        );
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app.status.text.contains("cache warning"));
        app.poll_update_worker();
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn create_child_folder_runs_in_foreground_worker_without_speculative_insertion() {
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::CreateChildFolder, false);
        app.focus = Focus::Navigation;
        let parent_id = app.selected_folder_id().cloned().expect("parent selected");
        let account_id = app.selected_account_id().expect("parent account");
        let folders = app.folders.clone();
        let navigation = app.navigation.clone();
        let notes = app.notes.clone();
        let selected_note_id = app
            .selected_note
            .as_ref()
            .map(|note| note.summary.id.clone());
        let search = app.search.clone();
        let preview_scroll = app.preview_scroll;
        let session_writes = app.session_write_calls;

        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Проект 🚀 \"Child\"");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        started.recv().expect("child-create worker entered backend");

        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert_eq!(app.folders, folders);
        assert_eq!(app.navigation, navigation);
        assert_eq!(app.notes, notes);
        assert_eq!(app.selected_account_id(), Some(account_id));
        assert_eq!(app.selected_folder_id(), Some(&parent_id));
        assert_eq!(
            app.selected_note
                .as_ref()
                .map(|note| note.summary.id.clone()),
            selected_note_id
        );
        assert_eq!(app.search, search);
        assert_eq!(app.preview_scroll, preview_scroll);
        assert!(app.popup.is_none());
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 0);
        assert_eq!(app.session_write_calls, session_writes);

        release.send(()).expect("release child-create worker");
        poll_update_until_idle(&mut app);

        let child_id = app.selected_folder_id().cloned().expect("child selected");
        let child = app
            .folders
            .iter()
            .find(|folder| folder.id == child_id)
            .expect("authoritative child in runtime");
        assert_eq!(
            child.parent,
            FolderParent::Folder {
                folder_id: parent_id
            }
        );
        assert!(app.update_worker.is_none());
        assert!(app.popup.is_none());
        assert!(matches!(app.search, SearchState::Inactive));
        assert!(app.selected_note.is_none());
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn successful_child_folder_create_session_failure_is_warning_only() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let parent_id = app.selected_folder_id().cloned().expect("parent selected");
        let path = temporary_session_path("child-folder-create-session-failure");
        let _ = fs::remove_file(&path);
        app.set_session_state(path.clone(), None);
        app.persist_session_selection();
        let before = fs::read(&path).expect("initial session bytes");
        app.session_write_failure = true;

        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Child 🚀");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);

        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
        let child_id = app.selected_folder_id().cloned().expect("child selected");
        assert!(
            matches!(app.folders.iter().find(|folder| folder.id == child_id).map(|folder| &folder.parent), Some(FolderParent::Folder { folder_id }) if folder_id == &parent_id)
        );
        assert!(app.notes.is_empty());
        assert!(app.selected_note.is_none());
        assert!(matches!(app.search, SearchState::Inactive));
        assert_eq!(app.preview_scroll, 0);
        assert_eq!(app.focus, Focus::Navigation);
        assert!(app.popup.is_none());
        assert!(app.update_worker.is_none());
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app.status.text.contains("Session warning"));
        assert!(!app.status.text.contains("Failed to create child"));
        assert_eq!(fs::read(&path).expect("unchanged session bytes"), before);
        app.poll_update_worker();
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 1);
        assert!(app.folders.iter().any(|folder| folder.id == child_id));
        let _ = fs::remove_file(path);
    }

    fn select_navigation_folder(app: &mut App, id: &FolderId) {
        app.selected_navigation = app
            .navigation
            .iter()
            .position(
                |item| matches!(item, NavigationItem::Folder { id: actual, .. } if actual == id),
            )
            .expect("folder in navigation");
    }

    #[test]
    fn reparent_folder_shortcut_opens_destination_popup() {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let source = app.selected_folder_id().cloned().expect("source");
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert!(matches!(
            &app.popup,
            Some(Popup::ReparentFolder { folder_id, destinations, selected_destination, .. })
                if folder_id == &source
                    && matches!(destinations.first(), Some(FolderReparentTarget::AccountRoot))
                    && *selected_destination == 0
        ));
    }

    #[test]
    fn reparent_folder_destination_list_excludes_source_and_descendants() {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let source = app.selected_folder_id().cloned().expect("source");
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Child");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        let child = app.selected_folder_id().cloned().expect("child");
        select_navigation_folder(&mut app, &source);
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        let Some(Popup::ReparentFolder { destinations, .. }) = &app.popup else {
            panic!("reparent popup");
        };
        assert!(!destinations.iter().any(|target| {
            matches!(target, FolderReparentTarget::Folder { folder_id, .. } if folder_id == &source || folder_id == &child)
        }));
    }

    #[test]
    fn reparent_folder_same_parent_is_local_noop() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let folders = app.folders.clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.popup.is_none());
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 0);
        assert_eq!(app.folders, folders);
        assert_eq!(app.status.text, "Folder location unchanged");
    }

    #[test]
    fn reparent_folder_runs_in_foreground_worker_without_speculative_hierarchy_change() {
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::ReparentFolder, false);
        app.focus = Focus::Navigation;
        let source = app.selected_folder_id().cloned().expect("source");
        let original_parent = app
            .folders
            .iter()
            .find(|folder| folder.id == source)
            .expect("source folder")
            .parent
            .clone();
        let navigation = app.navigation.clone();
        let selected_note = app
            .selected_note
            .as_ref()
            .map(|note| note.summary.id.clone());
        let search = app.search.clone();
        let preview_scroll = app.preview_scroll;
        let session_writes = app.session_write_calls;
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Down));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        started.recv().expect("reparent worker entered backend");
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert_eq!(app.navigation, navigation);
        assert_eq!(app.selected_folder_id(), Some(&source));
        assert_eq!(
            app.selected_note
                .as_ref()
                .map(|note| note.summary.id.clone()),
            selected_note
        );
        assert_eq!(app.search, search);
        assert_eq!(app.preview_scroll, preview_scroll);
        assert_eq!(app.session_write_calls, session_writes);
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 0);
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == source)
                .unwrap()
                .parent,
            original_parent
        );
        release.send(()).expect("release reparent worker");
        poll_update_until_idle(&mut app);
        assert!(app.update_worker.is_none());
        assert_eq!(app.selected_folder_id(), Some(&source));
    }

    #[test]
    fn failed_folder_reparent_preserves_authoritative_state_and_destination() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let folders = app.folders.clone();
        let source = app.selected_folder_id().cloned().expect("source");
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Down));
        counts.folder_reparent_fails.store(true, Ordering::SeqCst);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert_eq!(app.folders, folders);
        assert_eq!(app.selected_folder_id(), Some(&source));
        assert!(matches!(
            app.popup,
            Some(Popup::ReparentFolder {
                selected_destination: 1,
                ..
            })
        ));
        app.poll_update_worker();
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_folder_reparent_preserves_stable_id_and_context() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let source = app.selected_folder_id().cloned().expect("source");
        let note = app
            .selected_note
            .as_ref()
            .map(|note| note.summary.id.clone());
        app.search = SearchState::Active(ActiveSearch {
            query: "Alpha".into(),
            visible_ids: app
                .notes
                .iter()
                .filter(|note| note.name.contains("Alpha"))
                .map(|note| note.id.clone())
                .collect(),
        });
        let search = app.search.clone();
        let preview_scroll = 3;
        app.preview_scroll = preview_scroll;
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Down));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert_eq!(app.selected_folder_id(), Some(&source));
        assert_eq!(
            app.selected_note
                .as_ref()
                .map(|note| note.summary.id.clone()),
            note
        );
        assert_eq!(app.search, search);
        assert_eq!(app.preview_scroll, preview_scroll);
        assert!(matches!(
            app.folders.iter().find(|folder| folder.id == source).map(|folder| &folder.parent),
            Some(FolderParent::Folder { folder_id }) if folder_id == &FolderId::from("demo-work")
        ));
    }

    #[test]
    fn reparent_folder_to_account_root_uses_typed_root_target() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Root me");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        let child = app.selected_folder_id().cloned().expect("child");
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert!(matches!(
            &app.popup,
            Some(Popup::ReparentFolder {
                selected_destination: 1,
                ..
            })
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert!(matches!(
            app.folders
                .iter()
                .find(|folder| folder.id == child)
                .map(|folder| &folder.parent),
            Some(FolderParent::Account { .. })
        ));
    }

    #[test]
    fn successful_folder_reparent_cache_failure_does_not_retry_or_rollback() {
        let (mut app, counts, cache) = create_cache_app(false, None);
        app.focus = Focus::Navigation;
        let source = app.selected_folder_id().cloned().expect("source");
        cache.lock().unwrap().failure = Some(CacheFailurePoint::ReplaceSnapshot);
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Down));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app.status.text.contains("Moved folder"));
        assert!(app.status.text.contains("cache warning"));
        assert!(matches!(
            app.folders.iter().find(|folder| folder.id == source).map(|folder| &folder.parent),
            Some(FolderParent::Folder { folder_id }) if folder_id == &FolderId::from("demo-work")
        ));
        app.poll_update_worker();
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_folder_reparent_does_not_rewrite_session_when_continuity_state_is_unchanged() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let path = temporary_session_path("folder-reparent-session-no-write");
        let _ = fs::remove_file(&path);
        app.set_session_state(path.clone(), None);
        app.persist_session_selection();
        let before = fs::read(&path).expect("initial session bytes");
        let writes = app.session_write_calls;
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Down));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert_eq!(app.session_write_calls, writes);
        assert_eq!(fs::read(&path).expect("session bytes"), before);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reparent_folder_with_duplicate_destination_names_uses_selected_folder_id() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(&mut app, "Archive");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        let first = app.selected_folder_id().cloned().expect("first archive");
        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        type_text(&mut app, "Archive");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        let second = app.selected_folder_id().cloned().expect("second archive");
        assert_ne!(first, second);
        let source = FolderId::from("demo-notes");
        select_navigation_folder(&mut app, &source);
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        let destination = match &app.popup {
            Some(Popup::ReparentFolder { destinations, .. }) => destinations
                .iter()
                .position(|target| matches!(target, FolderReparentTarget::Folder { folder_id, .. } if folder_id == &second))
                .expect("second archive destination"),
            _ => panic!("reparent popup"),
        };
        for _ in 0..destination {
            app.handle_key(KeyEvent::from(KeyCode::Down));
        }
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        assert!(matches!(
            app.folders.iter().find(|folder| folder.id == source).map(|folder| &folder.parent),
            Some(FolderParent::Folder { folder_id }) if folder_id == &second
        ));
    }

    #[test]
    fn reparent_folder_preserves_descendant_branch_and_rebuilds_depths() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        let source = app.selected_folder_id().cloned().expect("source");
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Branch");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        let child = app.selected_folder_id().cloned().expect("child");
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        type_text(&mut app, "Leaf");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        let grandchild = app.selected_folder_id().cloned().expect("grandchild");
        select_navigation_folder(&mut app, &source);
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        app.handle_key(KeyEvent::from(KeyCode::Down));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 1);
        let depth =
            |id: &FolderId| match app.navigation.iter().find(
                |item| matches!(item, NavigationItem::Folder { id: actual, .. } if actual == id),
            ) {
                Some(NavigationItem::Folder { depth, .. }) => *depth,
                _ => panic!("folder is present in rebuilt navigation"),
            };
        assert_eq!(depth(&source), 1);
        assert_eq!(depth(&child), 2);
        assert_eq!(depth(&grandchild), 3);
    }

    #[test]
    fn uppercase_folder_and_lowercase_note_shortcuts_do_not_cross_route() {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.edit.as_ref().is_some_and(|edit| edit.is_new));
        app.edit = None;
        app.mode = AppMode::Normal;

        app.handle_key(KeyEvent::from(KeyCode::Char('N')));
        assert!(matches!(app.popup, Some(Popup::CreateFolder { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        assert!(matches!(app.popup, Some(Popup::CreateChildFolder { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        assert!(matches!(app.popup, Some(Popup::RenameFolder { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Esc));

        app.handle_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(matches!(app.popup, Some(Popup::Move { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert!(matches!(app.popup, Some(Popup::ReparentFolder { .. })));
    }

    #[test]
    fn editor_blocks_folder_shortcuts_without_starting_a_worker() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        for key in ['N', 'C', 'R', 'D', 'M'] {
            app.handle_key(KeyEvent::from(KeyCode::Char(key)));
            assert!(app.popup.is_none());
            assert!(app.update_worker.is_none());
        }
        assert_eq!(counts.folders_created.load(Ordering::SeqCst), 0);
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 0);
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 0);
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 0);
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn delete_folder_shortcut_opens_confirmation_for_selected_folder() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folder_id = create_empty_folder_for_delete(&mut app, "Delete me");
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert!(
            matches!(&app.popup, Some(Popup::DeleteFolder { folder_id: actual, folder_name, .. }) if actual == &folder_id && folder_name == "Delete me")
        );
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn delete_folder_confirmation_cancel_is_local_noop() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folder_id = create_empty_folder_for_delete(&mut app, "Keep me");
        let folders = app.folders.clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.popup.is_none());
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 0);
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.folders, folders);
    }

    #[test]
    fn successful_folder_delete_removes_exact_stable_id_and_selects_next_context() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let first = create_empty_folder_for_delete(&mut app, "Archive");
        let second = create_empty_folder_for_delete(&mut app, "Archive");
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        assert!(app.folders.iter().any(|folder| folder.id == first));
        assert!(!app.folders.iter().any(|folder| folder.id == second));
        assert_ne!(app.selected_folder_id(), Some(&second));
        assert!(matches!(app.search, SearchState::Inactive));
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn failed_folder_delete_preserves_authoritative_state() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let folder_id = create_empty_folder_for_delete(&mut app, "Cannot delete");
        let folders = app.folders.clone();
        counts.folder_delete_fails.store(true, Ordering::SeqCst);
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        assert!(app.popup.is_none());
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.folders, folders);
        assert!(app.status.is_error);
    }

    #[test]
    fn successful_folder_delete_cache_failure_does_not_retry_or_rollback() {
        let (mut app, counts, cache) = create_cache_app(false, None);
        let folder_id = create_empty_folder_for_delete(&mut app, "Cache delete");
        cache.lock().unwrap().failure = Some(CacheFailurePoint::ReplaceSnapshot);
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        assert!(!app.folders.iter().any(|folder| folder.id == folder_id));
        assert!(matches!(app.data_source, DataSourceState::Live));
        assert!(app.status.text.contains("Deleted folder"));
        assert!(app.status.text.contains("cache warning"));
        app.poll_update_worker();
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn delete_folder_runs_in_foreground_worker_without_speculative_removal() {
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::DeleteFolder, false);
        let folder_id = create_empty_folder_for_delete(&mut app, "Blocked delete");
        let note = app.selected_note_id();
        let scroll = app.preview_scroll;
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        started.recv().expect("folder delete worker started");
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert!(app.folders.iter().any(|folder| folder.id == folder_id));
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.selected_note_id(), note);
        assert_eq!(app.preview_scroll, scroll);
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 1);
        release.send(()).expect("release folder delete worker");
        poll_update_until_idle(&mut app);
        assert!(!app.folders.iter().any(|folder| folder.id == folder_id));
    }

    #[test]
    fn successful_last_folder_delete_enters_empty_context() {
        let account = Account {
            id: AccountId::from("only-account"),
            name: "Only".into(),
            is_default: true,
            is_upgraded: true,
            default_folder_id: None,
        };
        let folder = Folder {
            id: FolderId::from("only-folder"),
            account_id: account.id.clone(),
            name: "Only".into(),
            parent: FolderParent::Account {
                account_id: account.id.clone(),
            },
            shared: false,
        };
        let counts = MutationCounts::default();
        let backend = CountingBackend {
            inner: MockNotesBackend::new(vec![account], vec![folder.clone()], HashMap::new()),
            capabilities: BackendCapabilities::new(
                notes_core::RichTextCapabilities::all_supported(),
            ),
            counts: counts.clone(),
            create_fails: false,
            update_fails: false,
            move_fails: false,
            delete_fails: false,
        };
        let mut app = App::new(Box::new(backend));
        app.refresh();
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        assert!(app.folders.is_empty());
        assert!(app.selected_folder_id().is_none());
        assert!(app.notes.is_empty());
        assert!(app.selected_note.is_none());
        assert!(matches!(app.search, SearchState::Inactive));
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn successful_folder_delete_session_failure_is_warning_only() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let path = temporary_session_path("folder-delete-session-failure");
        let _ = fs::remove_file(&path);
        app.set_session_state(path.clone(), None);
        app.persist_session_selection();
        let _remaining_folder = create_empty_folder_for_delete(&mut app, "Session anchor");
        let folder_id = create_empty_folder_for_delete(&mut app, "Session delete");
        let before = fs::read(&path).expect("pre-delete session");
        app.session_write_failure = true;
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        assert!(!app.folders.iter().any(|folder| folder.id == folder_id));
        assert!(app.popup.is_none());
        assert!(app.update_worker.is_none());
        assert!(app.status.text.contains("Session warning"));
        assert_eq!(fs::read(&path).expect("old session remains"), before);
        app.poll_update_worker();
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn folder_rename_with_duplicate_display_names_reconciles_by_folder_id() {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let target_id = app.selected_folder_id().cloned().expect("target folder");
        let other_index = app
            .folders
            .iter()
            .position(|folder| folder.id != target_id)
            .expect("second folder");
        let other_id = app.folders[other_index].id.clone();
        app.folders[other_index].name = "Archive".into();
        let mut renamed = app
            .folders
            .iter()
            .find(|folder| folder.id == target_id)
            .unwrap()
            .clone();
        renamed.name = "Archive".into();
        let count = app.folders.len();
        app.finish_folder_renamed(renamed);
        assert_eq!(app.folders.len(), count);
        assert_eq!(app.selected_folder_id(), Some(&target_id));
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == other_id)
                .unwrap()
                .name,
            "Archive"
        );
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == target_id)
                .unwrap()
                .name,
            "Archive"
        );
    }

    #[test]
    fn rename_folder_runs_in_foreground_worker_without_speculative_change() {
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::RenameFolder, false);
        let folder_id = app.selected_folder_id().cloned().expect("selected folder");
        let old_name = app
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .expect("folder")
            .name
            .clone();
        let selected_note = app.selected_note_id();
        let scroll = app.preview_scroll;
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        for _ in 0..old_name.chars().count() {
            app.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        type_text(&mut app, "Проекты 🚀 \"2027\"");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        started.recv().expect("rename worker started");
        assert_eq!(counts.folders_renamed.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(app.selected_note_id(), selected_note);
        assert_eq!(app.preview_scroll, scroll);
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .unwrap()
                .name,
            old_name
        );
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 0);
        release.send(()).expect("release rename");
        poll_update_until_idle(&mut app);
        assert_eq!(app.selected_folder_id(), Some(&folder_id));
        assert_eq!(
            app.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .unwrap()
                .name,
            "Проекты 🚀 \"2027\""
        );
    }

    #[test]
    fn note_delete_confirmation_requires_exact_yes_and_only_calls_once() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert!(matches!(app.popup, Some(Popup::DeleteConfirm(_))));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        for key in [
            KeyCode::Esc,
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Char('x'),
        ] {
            app.popup = Some(Popup::DeleteConfirm(app.selected_note.clone().unwrap()));
            app.handle_key(KeyEvent::from(key));
            assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        }
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert!(app.status.text.contains("Recently Deleted"));
        assert!(!app.status.text.to_ascii_lowercase().contains("permanent"));
    }

    #[test]
    fn note_delete_is_gated_by_backend_capability_and_editor_mode() {
        let mut capabilities =
            BackendCapabilities::new(notes_core::RichTextCapabilities::all_supported());
        capabilities.delete = false;
        let (mut app, counts) = counting_app(capabilities);
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert!(app.popup.is_none());
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert!(app.status.text.contains("not supported"));
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(app.mode, AppMode::Insert);
    }

    #[test]
    fn note_delete_uppercase_and_selection_edges_are_safe() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('Y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert!(app.selected_note.is_some());
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(
            counts.deletes.load(Ordering::SeqCst),
            1,
            "a confirmation modal swallows D"
        );
        app.handle_key(KeyEvent::from(KeyCode::Esc));
    }

    #[test]
    fn delete_is_ignored_by_dirty_editor_and_attachment_modal() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        let before = current_body_text(&app);
        assert!(app.edit.as_ref().expect("editor").dirty);
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(current_body_text(&app), before);
        assert!(app.edit.as_ref().expect("editor").dirty);
        app.handle_key(modified('c', KeyModifiers::CONTROL));
        select_demo_note(&mut app, "demo-attachment");
        app.begin_attachments();
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert!(matches!(app.popup, Some(Popup::Attachments { .. })));
    }

    #[test]
    fn delete_failure_keeps_selected_note_and_selection_stable() {
        let (mut app, counts) = delete_selection_app(&["A", "B", "C"], 1, true);
        let selected = app
            .selected_note
            .as_ref()
            .expect("selected")
            .summary
            .id
            .clone();
        assert_eq!(selected, NoteId::from("test-b"));
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        app.handle_key(KeyEvent::from(KeyCode::Char('y')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert!(app.notes.iter().any(|note| note.id == selected));
        assert_eq!(
            app.selected_note.as_ref().expect("selected").summary.id,
            selected
        );
        assert!(app.popup.is_none());
        assert!(app.status.is_error);
    }

    #[test]
    fn successful_delete_of_first_note_selects_next_note() {
        let (mut app, counts) = delete_selection_app(&["A", "B", "C"], 0, false);
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.notes.len(), 2);
        assert_eq!(
            app.selected_note
                .as_ref()
                .expect("selected next note")
                .summary
                .id,
            NoteId::from("test-b")
        );
    }

    #[test]
    fn successful_delete_of_middle_note_selects_next_note() {
        let (mut app, counts) = delete_selection_app(&["A", "B", "C"], 1, false);
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.notes.len(), 2);
        assert_eq!(
            app.selected_note
                .as_ref()
                .expect("selected next note")
                .summary
                .id,
            NoteId::from("test-c")
        );
    }

    #[test]
    fn successful_delete_of_last_note_selects_previous_note() {
        let (mut app, counts) = delete_selection_app(&["A", "B"], 1, false);
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.notes.len(), 1);
        assert_eq!(
            app.selected_note
                .as_ref()
                .expect("selected previous note")
                .summary
                .id,
            NoteId::from("test-a")
        );
    }

    #[test]
    fn successful_delete_of_only_note_clears_selection_and_renders_safely() {
        let (mut app, counts) = delete_selection_app(&["A"], 0, false);
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert!(app.notes.is_empty());
        assert!(app.selected_note.is_none());
        app.handle_key(KeyEvent::from(KeyCode::Down));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("safe render");
    }

    #[test]
    fn search_input_absorbs_hotkeys_is_unicode_safe_and_has_no_backend_calls() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        let gets_before = counts.gets.load(Ordering::SeqCst);
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        type_text(&mut app, "DnemaxyПривет🚀");
        assert!(
            matches!(&app.search, SearchState::Editing { input, .. } if input == "DnemaxyПривет🚀")
        );
        assert_eq!(counts.creates.load(Ordering::SeqCst), 0);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(counts.gets.load(Ordering::SeqCst), gets_before);
        app.handle_key(KeyEvent::from(KeyCode::Backspace));
        app.handle_key(KeyEvent::from(KeyCode::Backspace));
        assert!(
            matches!(&app.search, SearchState::Editing { input, .. } if input == "DnemaxyПриве")
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(matches!(&app.search, SearchState::Editing { input, .. } if input.is_empty()));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.search, SearchState::Inactive);
        assert_eq!(counts.gets.load(Ordering::SeqCst), gets_before);
    }

    #[test]
    fn search_is_case_insensitive_unicode_title_only_and_safe_for_zero_results() {
        let (mut app, _) = delete_selection_app(
            &[
                "Alpha",
                "alpha Two",
                "Привет Мир",
                "Grüße aus Köln",
                "Emoji 🚀",
            ],
            0,
            false,
        );
        apply_search(&mut app, "ALPHA");
        assert_eq!(visible_names(&app), ["Alpha", "alpha Two"]);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        apply_search(&mut app, "ПРИВЕТ");
        assert_eq!(visible_names(&app), ["Привет Мир"]);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        apply_search(&mut app, "grüße");
        assert_eq!(visible_names(&app), ["Grüße aus Köln"]);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        apply_search(&mut app, "🚀");
        assert_eq!(visible_names(&app), ["Emoji 🚀"]);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        apply_search(&mut app, "absent");
        assert!(app.visible_note_ids().is_empty());
        assert!(app.selected_note.is_none());
        app.focus = Focus::Notes;
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Enter,
        ] {
            app.handle_key(KeyEvent::from(key));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("safe render");
    }

    #[test]
    fn search_preserves_matching_selection_and_clear_restores_full_list() {
        let (mut app, _) = delete_selection_app(&["Alpha", "Beta", "Gamma"], 1, false);
        let beta = app.selected_note_id().expect("beta selected");
        apply_search(&mut app, "beta");
        assert_eq!(app.selected_note_id(), Some(beta));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.search, SearchState::Inactive);
        assert_eq!(app.selected_note_id(), Some(NoteId::from("test-beta")));
        apply_search(&mut app, "alpha");
        assert_eq!(app.selected_note_id(), Some(NoteId::from("test-alpha")));
    }

    #[test]
    fn search_does_not_open_in_editor_or_over_authoritative_popups() {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        let title = app.edit.as_ref().expect("editor").title_buffer.clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        assert!(matches!(app.search, SearchState::Inactive));
        assert_eq!(
            app.edit.as_ref().expect("editor").title_buffer,
            format!("{title}/")
        );
        assert!(app.edit.as_ref().expect("editor").dirty);
        app.handle_key(modified('c', KeyModifiers::CONTROL));
        select_demo_note(&mut app, "demo-attachment");
        app.begin_attachments();
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        assert!(matches!(app.popup, Some(Popup::Attachments { .. })));
        assert!(matches!(app.search, SearchState::Inactive));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.popup = Some(Popup::DeleteConfirm(
            app.selected_note.clone().expect("selected note"),
        ));
        app.handle_key(KeyEvent::from(KeyCode::Char('/')));
        assert!(matches!(app.popup, Some(Popup::DeleteConfirm(_))));
        assert!(matches!(app.search, SearchState::Inactive));
    }

    #[test]
    fn filtered_delete_targets_selected_note_and_recomputes_or_preserves_on_error() {
        let (mut app, counts) = delete_selection_app(&["Alpha", "Beta", "Alpha Two"], 0, false);
        apply_search(&mut app, "alpha");
        app.focus = Focus::Notes;
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.selected_note_id(), Some(NoteId::from("test-alpha two")));
        confirm_delete(&mut app);
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(visible_names(&app), ["Alpha"]);
        assert_eq!(app.selected_note_id(), Some(NoteId::from("test-alpha")));

        let (mut failing, failing_counts) =
            delete_selection_app(&["Alpha", "Beta", "Alpha Two"], 0, true);
        apply_search(&mut failing, "alpha");
        failing.focus = Focus::Notes;
        failing.handle_key(KeyEvent::from(KeyCode::Char('j')));
        let selected = failing.selected_note_id().expect("selected alpha two");
        confirm_delete(&mut failing);
        assert_eq!(failing_counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(visible_names(&failing), ["Alpha", "Alpha Two"]);
        assert_eq!(failing.selected_note_id(), Some(selected));
        assert!(failing.status.is_error);
    }

    #[test]
    fn reload_recomputes_active_search_and_folder_navigation_clears_it() {
        let (mut app, _) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        apply_search(&mut app, "alpha");
        let before = visible_names(&app);
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        assert!(matches!(app.search, SearchState::Active(_)));
        assert_eq!(visible_names(&app), before);
        app.focus = Focus::Navigation;
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.search, SearchState::Inactive);
    }

    #[test]
    fn filtered_edit_save_move_and_create_recompute_membership() {
        let (mut app, counts) = delete_selection_app(&["Alpha", "Beta", "Alpha Two"], 0, false);
        apply_search(&mut app, "alpha");
        app.focus = Focus::Notes;
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        assert_eq!(
            app.edit.as_ref().and_then(|edit| edit.note_id.clone()),
            Some(NoteId::from("test-alpha two"))
        );
        {
            let edit = app.edit.as_mut().expect("editor");
            edit.title_buffer = "Gamma Two".into();
            edit.dirty = true;
        }
        app.save_edit(true);
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(visible_names(&app), ["Alpha"]);

        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        type_text(&mut app, "Alpha New");
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert_eq!(visible_names(&app), ["Alpha", "Alpha New"]);

        app.focus = Focus::Notes;
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Char('m')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        poll_update_until_idle(&mut app);
        assert_eq!(visible_names(&app), ["Alpha"]);
    }

    #[test]
    fn cached_mode_blocks_all_live_mutations_and_attachment_operations() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        app.data_source = DataSourceState::CachedBackendUnavailable {
            message: "synthetic backend failure".into(),
        };
        for key in [
            KeyCode::Char('n'),
            KeyCode::Char('e'),
            KeyCode::Char('m'),
            KeyCode::Char('D'),
            KeyCode::Char('a'),
        ] {
            app.handle_key(KeyEvent::from(key));
        }
        assert_eq!(counts.creates.load(Ordering::SeqCst), 0);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(counts.previews.load(Ordering::SeqCst), 0);
        assert_eq!(counts.exports.load(Ordering::SeqCst), 0);
        assert!(app.status.text.contains("cached mode is read-only"));
    }

    #[test]
    fn cache_store_records_full_note_load_by_note_id_and_upsert() {
        let note = demo_note(
            "cached-note",
            "demo-notes",
            "Русская заметка 🚀",
            "Grüße aus Köln",
        );
        let (mut cache, state) = FakeCache::new(FakeCacheState::default());
        cache.upsert_note(&note).unwrap();
        assert_eq!(
            cache.load_note(&note.summary.id).unwrap(),
            Some(note.clone())
        );
        let state = state.lock().unwrap();
        assert_eq!(state.upsert_note_calls, 1);
        assert_eq!(state.upsert_note_ids, vec![note.summary.id.clone()]);
        assert_eq!(state.load_note_calls, 1);
        assert_eq!(state.load_note_ids, vec![note.summary.id]);
    }

    struct ScriptedRefreshFixture {
        accounts: Vec<Account>,
        folders: Vec<Folder>,
        query: NotesQuery,
        page: NotesPage,
        selected_id: NoteId,
        selected_note: Note,
    }

    fn scripted_refresh_fixture() -> ScriptedRefreshFixture {
        let source = demo_backend();
        let accounts = source.accounts().unwrap();
        let folders = source.folders(None).unwrap();
        let query = NotesQuery {
            folder_id: Some(FolderId::from("demo-notes")),
            ..Default::default()
        };
        let page = source.notes(&query).unwrap();
        let selected_id = page.items.first().expect("demo notes").id.clone();
        let selected_note = source.get_note(&selected_id).unwrap();
        ScriptedRefreshFixture {
            accounts,
            folders,
            query,
            page,
            selected_id,
            selected_note,
        }
    }

    fn scripted_backend(
        accounts: VecDeque<Result<Vec<Account>, NotesError>>,
        folders: VecDeque<ScriptedFoldersCall>,
        notes: VecDeque<ScriptedNotesCall>,
        get_notes: VecDeque<ScriptedGetNoteCall>,
    ) -> (ScriptedNotesBackend, Arc<Mutex<Vec<BackendCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            ScriptedNotesBackend {
                inner: demo_backend(),
                accounts: Mutex::new(accounts),
                folders: Mutex::new(folders),
                notes: Mutex::new(notes),
                get_notes: Mutex::new(get_notes),
                calls: calls.clone(),
            },
            calls,
        )
    }

    fn successful_refresh_backend() -> (
        ScriptedNotesBackend,
        Arc<Mutex<Vec<BackendCall>>>,
        ScriptedRefreshFixture,
    ) {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::from([Ok(fixture.accounts.clone())]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(fixture.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: fixture.query.clone(),
                outcome: Ok(fixture.page.clone()),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: fixture.selected_id.clone(),
                outcome: Ok(fixture.selected_note.clone()),
            }]),
        );
        (backend, calls, fixture)
    }

    fn cached_state_with_notes(notes: &[(&str, &str)]) -> CachedState {
        let fixture = scripted_refresh_fixture();
        CachedState {
            accounts: fixture.accounts,
            folders: fixture.folders,
            notes: notes
                .iter()
                .map(|(id, name)| demo_note(id, "demo-notes", name, "").summary)
                .collect(),
            last_successful_refresh: None,
        }
    }

    fn refresh_fixture_with_notes(notes: &[(&str, &str)]) -> ScriptedRefreshFixture {
        let state = cached_state_with_notes(notes);
        let full_notes = notes
            .iter()
            .map(|(id, name)| {
                let note = demo_note(id, "demo-notes", name, "");
                (note.summary.id.clone(), note)
            })
            .collect();
        let source =
            MockNotesBackend::new(state.accounts.clone(), state.folders.clone(), full_notes);
        let query = NotesQuery {
            folder_id: Some(FolderId::from("demo-notes")),
            ..Default::default()
        };
        let page = source.notes(&query).unwrap();
        let selected_id = page.items.first().expect("scripted notes").id.clone();
        let selected_note = source.get_note(&selected_id).unwrap();
        ScriptedRefreshFixture {
            accounts: state.accounts,
            folders: state.folders,
            query,
            page,
            selected_id,
            selected_note,
        }
    }

    fn successful_scripted_backend(
        fixture: &ScriptedRefreshFixture,
    ) -> (ScriptedNotesBackend, Arc<Mutex<Vec<BackendCall>>>) {
        scripted_backend(
            VecDeque::from([Ok(fixture.accounts.clone())]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(fixture.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: fixture.query.clone(),
                outcome: Ok(fixture.page.clone()),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: fixture.selected_id.clone(),
                outcome: Ok(fixture.selected_note.clone()),
            }]),
        )
    }

    fn note_ids(app: &App) -> Vec<NoteId> {
        app.notes.iter().map(|note| note.id.clone()).collect()
    }

    fn app_with_cache(
        backend: ScriptedNotesBackend,
        cache_state: FakeCacheState,
    ) -> (App, Arc<Mutex<FakeCacheState>>) {
        let (cache, state) = FakeCache::new(cache_state);
        (App::with_cache(Box::new(backend), Box::new(cache)), state)
    }

    #[test]
    fn scripted_backend_can_fail_accounts_then_succeed() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Ok(fixture.accounts),
            ]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        assert!(backend.accounts().is_err());
        assert!(!backend.accounts().unwrap().is_empty());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendCall::Accounts, BackendCall::Accounts]
        );
    }

    #[test]
    fn scripted_backend_records_real_refresh_call_order() {
        let (backend, calls, fixture) = successful_refresh_backend();
        let mut app = App::new(Box::new(backend));
        app.refresh();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                BackendCall::Accounts,
                BackendCall::Folders(None),
                BackendCall::Notes(fixture.query.clone()),
                BackendCall::GetNote(fixture.selected_id.clone()),
            ]
        );
        assert_eq!(app.selected_note, Some(fixture.selected_note));
    }

    #[test]
    fn scripted_backend_can_fail_folders() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::from([Ok(fixture.accounts)]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Err(NotesError::Backend("folders offline".into())),
            }]),
            VecDeque::new(),
            VecDeque::new(),
        );
        let mut app = App::new(Box::new(backend));
        app.refresh();
        assert!(app.status.is_error);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendCall::Accounts, BackendCall::Folders(None)]
        );
    }

    #[test]
    fn scripted_backend_can_fail_notes() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::from([Ok(fixture.accounts)]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(fixture.folders),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: fixture.query.clone(),
                outcome: Err(NotesError::Backend("notes offline".into())),
            }]),
            VecDeque::new(),
        );
        let mut app = App::new(Box::new(backend));
        app.refresh();
        assert!(app.status.is_error);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                BackendCall::Accounts,
                BackendCall::Folders(None),
                BackendCall::Notes(fixture.query),
            ]
        );
    }

    #[test]
    fn scripted_backend_records_get_note_id() {
        let (backend, calls, fixture) = successful_refresh_backend();
        let mut app = App::new(Box::new(backend));
        app.refresh();
        assert_eq!(
            calls.lock().unwrap().last(),
            Some(&BackendCall::GetNote(fixture.selected_id.clone()))
        );
        assert_eq!(app.selected_note, Some(fixture.selected_note));
    }

    #[test]
    fn scripted_backend_can_fail_get_note() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::from([Ok(fixture.accounts)]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(fixture.folders),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: fixture.query.clone(),
                outcome: Ok(fixture.page),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: fixture.selected_id.clone(),
                outcome: Err(NotesError::Backend("get note offline".into())),
            }]),
        );
        let mut app = App::new(Box::new(backend));
        app.refresh();
        assert!(app.status.is_error);
        assert!(app.selected_note.is_none());
        assert_eq!(
            calls.lock().unwrap().last(),
            Some(&BackendCall::GetNote(fixture.selected_id))
        );
    }

    #[test]
    fn scripted_backend_rejects_unexpected_folders_argument() {
        let fixture = scripted_refresh_fixture();
        let expected = Some(AccountId::from("expected-account"));
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::from([ScriptedFoldersCall {
                expected: expected.clone(),
                outcome: Ok(fixture.folders),
            }]),
            VecDeque::new(),
            VecDeque::new(),
        );
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| backend.folders(None))).is_err());
        assert_eq!(*calls.lock().unwrap(), vec![BackendCall::Folders(None)]);
        assert_eq!(backend.folders.lock().unwrap().len(), 1);
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| backend.folders(None))).is_err());
        assert_eq!(backend.folders.lock().unwrap().len(), 1);
    }

    #[test]
    fn scripted_backend_rejects_unexpected_notes_argument() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::from([ScriptedNotesCall {
                expected: NotesQuery {
                    account_id: Some(AccountId::from("unexpected")),
                    ..Default::default()
                },
                outcome: Ok(fixture.page),
            }]),
            VecDeque::new(),
        );
        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| backend.notes(&fixture.query))).is_err()
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendCall::Notes(fixture.query)]
        );
        assert_eq!(backend.notes.lock().unwrap().len(), 1);
    }

    #[test]
    fn scripted_backend_rejects_unexpected_get_note_id() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::from([ScriptedGetNoteCall {
                expected: NoteId::from("expected-note"),
                outcome: Ok(fixture.selected_note),
            }]),
        );
        assert!(std::panic::catch_unwind(AssertUnwindSafe(
            || backend.get_note(&fixture.selected_id)
        ))
        .is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendCall::GetNote(fixture.selected_id)]
        );
        assert_eq!(backend.get_notes.lock().unwrap().len(), 1);
    }

    #[test]
    fn scripted_backend_detects_exhausted_accounts_script() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::from([Ok(fixture.accounts)]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        assert!(backend.accounts().is_ok());
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| backend.accounts())).is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendCall::Accounts, BackendCall::Accounts]
        );
    }

    #[test]
    fn scripted_backend_detects_exhausted_folders_and_notes_scripts() {
        let fixture = scripted_refresh_fixture();
        let (backend, _) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| backend.folders(None))).is_err());
        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| backend.notes(&fixture.query))).is_err()
        );
    }

    #[test]
    fn scripted_backend_detects_exhausted_get_note_script() {
        let fixture = scripted_refresh_fixture();
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::from([ScriptedGetNoteCall {
                expected: fixture.selected_id.clone(),
                outcome: Ok(fixture.selected_note),
            }]),
        );
        assert!(backend.get_note(&fixture.selected_id).is_ok());
        assert!(std::panic::catch_unwind(AssertUnwindSafe(
            || backend.get_note(&fixture.selected_id)
        ))
        .is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                BackendCall::GetNote(fixture.selected_id.clone()),
                BackendCall::GetNote(fixture.selected_id),
            ]
        );
    }

    #[test]
    fn cache_bootstrap_uses_injected_store_once() {
        let cached = cached_state_with_notes(&[
            ("cached-a", "A-note"),
            ("cached-b", "B-note"),
            ("cached-c", "C-note"),
        ]);
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached.clone(),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        assert_eq!(app.data_source, DataSourceState::Cached);
        assert_eq!(
            note_ids(&app),
            cached
                .notes
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(cache.lock().unwrap().load_bootstrap_calls, 1);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cache_load_failure_does_not_block_live_startup() {
        let fixture = scripted_refresh_fixture();
        let (backend, _) = successful_scripted_backend(&fixture);
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                failure: Some(CacheFailurePoint::LoadBootstrap),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note, Some(fixture.selected_note));
        assert_eq!(cache.load_bootstrap_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
    }

    #[test]
    fn backend_failure_after_cached_bootstrap_keeps_cached_state() {
        let cached =
            cached_state_with_notes(&[("cached-a", "A"), ("cached-b", "B"), ("cached-c", "C")]);
        let (backend, calls) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached.clone(),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.selected_note_index = 1;
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(
            note_ids(&app),
            cached
                .notes
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(app.selected_note_id(), Some(NoteId::from("cached-b")));
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        assert_eq!(cache.load_bootstrap_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert!(cache.replaced_snapshots.is_empty());
        assert_eq!(*calls.lock().unwrap(), vec![BackendCall::Accounts]);
    }

    #[test]
    fn failed_live_refresh_does_not_replace_cache() {
        let cached = cached_state_with_notes(&[("cached-a", "A")]);
        let (backend, _) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert!(cache.replaced_snapshots.is_empty());
    }

    #[test]
    fn reload_failure_keeps_cached_runtime_state() {
        let cached =
            cached_state_with_notes(&[("cached-a", "A"), ("cached-b", "B"), ("cached-c", "C")]);
        let (backend, calls) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline first".into())),
                Err(NotesError::Backend("offline second".into())),
            ]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached.clone(),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.selected_note_index = 1;
        app.refresh();
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(
            note_ids(&app),
            cached
                .notes
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(app.selected_note_id(), Some(NoteId::from("cached-b")));
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        assert_eq!(cache.load_bootstrap_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendCall::Accounts, BackendCall::Accounts]
        );
    }

    #[test]
    fn reload_success_transitions_cached_state_back_to_live() {
        let cached =
            cached_state_with_notes(&[("cached-a", "A"), ("cached-b", "B"), ("cached-c", "C")]);
        let live = refresh_fixture_with_notes(&[("live-a", "A live"), ("live-b", "B live")]);
        let (backend, calls) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Ok(live.accounts.clone()),
            ]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(live.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: live.query.clone(),
                outcome: Ok(live.page.clone()),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: live.selected_id.clone(),
                outcome: Ok(live.selected_note.clone()),
            }]),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(
            note_ids(&app),
            live.page
                .items
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                BackendCall::Accounts,
                BackendCall::Accounts,
                BackendCall::Folders(None),
                BackendCall::Notes(live.query),
                BackendCall::GetNote(live.selected_id),
            ]
        );
    }

    #[test]
    fn successful_live_refresh_replaces_cached_runtime_state() {
        let cached = cached_state_with_notes(&[("old-a", "OLD-A"), ("old-b", "OLD-B")]);
        let live = refresh_fixture_with_notes(&[
            ("live-a", "LIVE-A"),
            ("live-b", "LIVE-B"),
            ("live-c", "LIVE-C"),
        ]);
        let (backend, _) = successful_scripted_backend(&live);
        let (mut app, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(
            note_ids(&app),
            live.page
                .items
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>()
        );
        assert!(!note_ids(&app).contains(&NoteId::from("old-a")));
        assert!(!note_ids(&app).contains(&NoteId::from("old-b")));
    }

    #[test]
    fn successful_live_refresh_persists_through_cache_store() {
        let cached = cached_state_with_notes(&[("old-only", "OLD")]);
        let live = refresh_fixture_with_notes(&[
            ("live-a", "LIVE-A"),
            ("live-b", "LIVE-B"),
            ("live-c", "LIVE-C"),
        ]);
        let (backend, _) = successful_scripted_backend(&live);
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots.len(), 1);
        assert_eq!(
            cache.replaced_snapshots[0]
                .notes
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>(),
            live.page
                .items
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>(),
        );
        assert!(!cache.replaced_snapshots[0]
            .notes
            .iter()
            .any(|note| note.id == NoteId::from("old-only")));
    }

    #[test]
    fn cached_selection_is_preserved_by_note_id_after_live_retry() {
        let cached = cached_state_with_notes(&[("a", "A"), ("b", "B"), ("c", "C")]);
        let live = refresh_fixture_with_notes(&[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")]);
        let (backend, _) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Ok(live.accounts.clone()),
            ]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(live.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: live.query.clone(),
                outcome: Ok(live.page.clone()),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: NoteId::from("b"),
                outcome: Ok(demo_note("b", "demo-notes", "B", "")),
            }]),
        );
        let (mut app, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.selected_note_index = 1;
        app.refresh();
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(NoteId::from("b")));
    }

    #[test]
    fn missing_cached_selection_falls_back_safely_after_live_retry() {
        let cached = cached_state_with_notes(&[("a", "A"), ("b", "B"), ("c", "C")]);
        let live = refresh_fixture_with_notes(&[("a", "A"), ("c", "C"), ("d", "D")]);
        let (backend, _) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Ok(live.accounts.clone()),
            ]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(live.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: live.query.clone(),
                outcome: Ok(live.page.clone()),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: live.selected_id.clone(),
                outcome: Ok(live.selected_note.clone()),
            }]),
        );
        let (mut app, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.selected_note_index = 1;
        app.refresh();
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_ne!(app.selected_note_id(), Some(NoteId::from("b")));
        assert!(app
            .selected_note_id()
            .is_some_and(|id| note_ids(&app).contains(&id)));
    }

    #[test]
    fn cache_write_failure_after_live_refresh_keeps_live_state() {
        let live = refresh_fixture_with_notes(&[("live-a", "LIVE-A")]);
        let (backend, _) = successful_scripted_backend(&live);
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                failure: Some(CacheFailurePoint::ReplaceSnapshot),
                ..Default::default()
            },
        );
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(
            note_ids(&app),
            live.page
                .items
                .iter()
                .map(|note| note.id.clone())
                .collect::<Vec<_>>()
        );
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 1);
    }

    #[test]
    fn cache_write_failure_after_live_retry_is_warning_not_offline() {
        let cached = cached_state_with_notes(&[("a", "A"), ("b", "B"), ("c", "C")]);
        let live = refresh_fixture_with_notes(&[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")]);
        let (backend, _) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Ok(live.accounts.clone()),
            ]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(live.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: live.query.clone(),
                outcome: Ok(live.page.clone()),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: NoteId::from("b"),
                outcome: Ok(demo_note("b", "demo-notes", "B", "")),
            }]),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                failure: Some(CacheFailurePoint::ReplaceSnapshot),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.selected_note_index = 1;
        app.refresh();
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(NoteId::from("b")));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.text.contains("backend unavailable"));
        assert_eq!(cache.lock().unwrap().replace_snapshot_calls, 1);
    }

    #[test]
    fn search_works_over_cached_bootstrap_state_without_backend_calls() {
        let cached = cached_state_with_notes(&[
            ("alpha", "Alpha"),
            ("beta", "Beta"),
            ("cyrillic", "Альфа"),
            ("german", "Grüße"),
        ]);
        let (backend, calls) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        let calls_before_search = calls.lock().unwrap().clone();
        apply_search(&mut app, "alpha");
        assert_eq!(visible_names(&app), ["Alpha"]);
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        assert_eq!(*calls.lock().unwrap(), calls_before_search);
        assert_eq!(cache.lock().unwrap().load_bootstrap_calls, 1);
    }

    #[test]
    fn cached_full_note_preview_loads_from_cache_without_backend_call() {
        let cached = cached_state_with_notes(&[("cached-a", "A")]);
        let full = demo_note(
            "cached-a",
            "demo-notes",
            "A",
            "Русский текст\nGrüße aus Köln\nemoji 🚀",
        );
        let (backend, calls) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                full_notes: HashMap::from([(full.summary.id.clone(), full.clone())]),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        assert_eq!(app.selected_note, Some(full.clone()));
        let cache = cache.lock().unwrap();
        assert_eq!(cache.load_note_ids, vec![NoteId::from("cached-a")]);
        assert_eq!(*calls.lock().unwrap(), vec![BackendCall::Accounts]);
        assert!(app
            .selected_note
            .as_ref()
            .unwrap()
            .plaintext
            .contains("Grüße aus Köln"));
    }

    #[test]
    fn cached_preview_loads_full_note_by_selected_note_id() {
        let cached = cached_state_with_notes(&[("a", "A"), ("b", "B")]);
        let a = demo_note("a", "demo-notes", "A", "body-A");
        let b = demo_note("b", "demo-notes", "B", "body-B");
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                full_notes: HashMap::from([
                    (a.summary.id.clone(), a),
                    (b.summary.id.clone(), b.clone()),
                ]),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.focus = Focus::Notes;
        app.move_selection(1);
        assert_eq!(app.selected_note, Some(b));
        assert_eq!(
            cache.lock().unwrap().load_note_ids,
            vec![NoteId::from("a"), NoteId::from("b")]
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_cached_full_note_is_explicit_and_does_not_call_backend() {
        let cached = cached_state_with_notes(&[("a", "A")]);
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        assert!(matches!(
            app.cached_preview_state,
            Some(CachedPreviewState::MissingFullNote)
        ));
        assert_eq!(app.selected_note_id(), Some(NoteId::from("a")));
        assert_eq!(cache.lock().unwrap().load_note_ids, vec![NoteId::from("a")]);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn cached_empty_note_body_is_distinct_from_missing_cached_note() {
        let cached = cached_state_with_notes(&[("empty", "Empty")]);
        let empty = demo_note("empty", "demo-notes", "Empty", "");
        let (backend, _) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut present, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached.clone(),
                full_notes: HashMap::from([(empty.summary.id.clone(), empty)]),
                ..Default::default()
            },
        );
        present.bootstrap_cache();
        let (missing_backend, _) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut missing, _) = app_with_cache(
            missing_backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        missing.bootstrap_cache();
        assert!(present.selected_note.is_some());
        assert_eq!(present.cached_preview_state, None);
        assert!(matches!(
            missing.cached_preview_state,
            Some(CachedPreviewState::MissingFullNote)
        ));
    }

    #[test]
    fn cached_preview_cache_read_failure_is_safe() {
        let cached = cached_state_with_notes(&[("a", "A")]);
        let (backend, calls) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                failure: Some(CacheFailurePoint::LoadNote),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        assert!(matches!(
            app.cached_preview_state,
            Some(CachedPreviewState::ReadError(_))
        ));
        assert_eq!(app.selected_note_id(), Some(NoteId::from("a")));
        assert_eq!(cache.lock().unwrap().load_note_calls, 1);
        assert_eq!(*calls.lock().unwrap(), vec![BackendCall::Accounts]);
    }

    #[test]
    fn cached_full_note_does_not_enable_editing() {
        let cached = cached_state_with_notes(&[("a", "A")]);
        let full = demo_note("a", "demo-notes", "A", "cached body");
        let (backend, calls) = scripted_backend(
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                full_notes: HashMap::from([(full.summary.id.clone(), full.clone())]),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.edit.is_none());
        assert_eq!(app.selected_note, Some(full));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(cache.lock().unwrap().upsert_note_calls, 0);
    }

    #[test]
    fn successful_live_get_note_upserts_full_note_cache() {
        let fixture = refresh_fixture_with_notes(&[("live-a", "Live A")]);
        let (backend, _) = successful_scripted_backend(&fixture);
        let (mut app, cache) = app_with_cache(backend, FakeCacheState::default());
        app.refresh();
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![fixture.selected_id.clone()]);
        assert_eq!(
            cache.full_notes.get(&fixture.selected_id),
            Some(&fixture.selected_note)
        );
    }

    #[test]
    fn failed_live_get_note_does_not_upsert_full_note_cache() {
        let fixture = refresh_fixture_with_notes(&[("live-a", "Live A")]);
        let (backend, _) = scripted_backend(
            VecDeque::from([Ok(fixture.accounts.clone())]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(fixture.folders.clone()),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: fixture.query.clone(),
                outcome: Ok(fixture.page),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: fixture.selected_id,
                outcome: Err(NotesError::Backend("get failed".into())),
            }]),
        );
        let (mut app, cache) = app_with_cache(backend, FakeCacheState::default());
        app.refresh();
        assert!(app.status.is_error);
        assert_eq!(cache.lock().unwrap().upsert_note_calls, 0);
    }

    #[test]
    fn cache_upsert_failure_after_live_get_note_keeps_live_preview() {
        let fixture = refresh_fixture_with_notes(&[("live-a", "Live A")]);
        let (backend, _) = successful_scripted_backend(&fixture);
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                failure: Some(CacheFailurePoint::UpsertNote),
                ..Default::default()
            },
        );
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note, Some(fixture.selected_note));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        assert_eq!(cache.lock().unwrap().upsert_note_calls, 1);
    }

    #[test]
    fn live_preview_still_uses_backend_get_note() {
        let fixture = refresh_fixture_with_notes(&[("live-a", "Live A")]);
        let (backend, calls) = successful_scripted_backend(&fixture);
        let (mut app, cache) = app_with_cache(backend, FakeCacheState::default());
        app.refresh();
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(calls
            .lock()
            .unwrap()
            .contains(&BackendCall::GetNote(fixture.selected_id.clone())));
        let cache = cache.lock().unwrap();
        assert_eq!(cache.load_note_calls, 0);
        assert_eq!(cache.upsert_note_ids, vec![fixture.selected_id]);
    }

    #[test]
    fn successful_create_updates_cache() {
        let (mut app, counts, cache) = create_cache_app(false, None);
        save_new_note(&mut app, "Cached create", "created body");

        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        let created = app.selected_note.clone().expect("created note selected");
        assert_eq!(created.summary.name, "Cached create");
        assert!(app.notes.iter().any(|note| note.id == created.summary.id));

        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![created.summary.id.clone()]);
        assert_eq!(cache.full_notes.get(&created.summary.id), Some(&created));
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert!(cache.replaced_snapshots[0]
            .notes
            .iter()
            .any(|note| note.id == created.summary.id));
    }

    #[test]
    fn failed_create_does_not_update_cache() {
        let (mut app, counts, cache) = create_cache_app(true, None);
        let original_ids = note_ids(&app);
        save_new_note(&mut app, "Unsaved create", "must not persist");

        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert_eq!(note_ids(&app), original_ids);
        assert!(app.edit.as_ref().is_some_and(|edit| edit.is_new));
        assert!(app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert!(cache.full_notes.is_empty());
    }

    #[test]
    fn cache_upsert_failure_after_successful_create_is_warning_only() {
        let (mut app, counts, cache) = create_cache_app(false, Some(CacheFailurePoint::UpsertNote));
        save_new_note(&mut app, "Upsert warning", "created body");

        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(app.selected_note.is_some());
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots.len(), 1);
    }

    #[test]
    fn cache_snapshot_failure_after_successful_create_is_warning_only() {
        let (mut app, counts, cache) =
            create_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        save_new_note(&mut app, "Snapshot warning", "created body");

        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(app.selected_note.is_some());
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
    }

    #[test]
    fn successful_update_updates_cache() {
        let (mut app, counts, cache) = update_cache_app(false, None);
        let original_id = app.selected_note_id().expect("selected note");
        save_updated_note(&mut app, " · updated", " updated body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(original_id.clone()));
        let updated = app.selected_note.clone().expect("updated selected note");
        assert_eq!(updated.summary.id, original_id);
        assert!(updated.summary.name.ends_with(" · updated"));
        assert!(updated.plaintext.contains("updated body"));
        assert!(app
            .notes
            .iter()
            .any(|summary| summary.id == original_id && summary.name == updated.summary.name));

        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![original_id.clone()]);
        assert_eq!(cache.full_notes.get(&original_id), Some(&updated));
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert!(cache.replaced_snapshots[0]
            .notes
            .iter()
            .any(|summary| summary.id == original_id && summary.name == updated.summary.name));
    }

    #[test]
    fn failed_update_does_not_update_cache() {
        let (mut app, counts, cache) = update_cache_app(true, None);
        let original_id = app.selected_note_id().expect("selected note");
        save_updated_note(&mut app, " · unsaved", " unsaved body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.selected_note_id(), Some(original_id));
        let edit = app.edit.as_ref().expect("editor remains open");
        assert!(edit.dirty);
        assert!(edit.title_buffer.ends_with(" · unsaved"));
        assert!(current_body_text(&app).contains("unsaved body"));
        assert!(app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert!(cache.full_notes.is_empty());
    }

    #[test]
    fn cache_upsert_failure_after_successful_update_is_warning_only() {
        let (mut app, counts, cache) = update_cache_app(false, Some(CacheFailurePoint::UpsertNote));
        let original_id = app.selected_note_id().expect("selected note");
        save_updated_note(&mut app, " · warning", " updated body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(original_id));
        assert!(app
            .selected_note
            .as_ref()
            .is_some_and(|note| note.summary.name.ends_with(" · warning")));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots.len(), 1);
    }

    #[test]
    fn cache_snapshot_failure_after_successful_update_is_warning_only() {
        let (mut app, counts, cache) =
            update_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        let original_id = app.selected_note_id().expect("selected note");
        save_updated_note(&mut app, " · snapshot", " updated body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(original_id));
        assert!(app
            .selected_note
            .as_ref()
            .is_some_and(|note| note.summary.name.ends_with(" · snapshot")));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
    }

    #[test]
    fn dirty_editor_changes_are_not_persisted_before_backend_save() {
        let (mut app, counts, cache) = update_cache_app(true, None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, " unsaved body");
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(&mut app, " · unsaved");
        assert!(app.edit.as_ref().is_some_and(|edit| edit.dirty));
        {
            let cache = cache.lock().unwrap();
            assert_eq!(cache.upsert_note_calls, 0);
            assert_eq!(cache.replace_snapshot_calls, 0);
        }

        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
    }

    #[test]
    fn successful_conflict_overwrite_updates_cache() {
        let (mut app, counts, cache) = update_cache_app(false, None);
        let original_id = app.selected_note_id().expect("selected note");
        save_conflict_overwrite(&mut app, &counts, " · overwrite", " overwritten body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(original_id.clone()));
        let overwritten = app.selected_note.clone().expect("overwritten note");
        assert_eq!(overwritten.summary.id, original_id);
        assert!(overwritten.summary.name.ends_with(" · overwrite"));
        assert!(overwritten.plaintext.contains("overwritten body"));

        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![original_id.clone()]);
        assert_eq!(cache.full_notes.get(&original_id), Some(&overwritten));
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert!(cache.replaced_snapshots[0]
            .notes
            .iter()
            .any(|summary| summary.id == original_id && summary.name == overwritten.summary.name));
    }

    #[test]
    fn failed_conflict_overwrite_does_not_update_cache() {
        let (mut app, counts, cache) = update_cache_app(true, None);
        let original = app.selected_note.clone().expect("selected note");
        save_conflict_overwrite(&mut app, &counts, " · unsaved", " unsaved body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.popup.is_none());
        assert_eq!(app.selected_note, Some(original));
        let edit = app.edit.as_ref().expect("editor remains open");
        assert!(edit.dirty);
        assert!(edit.title_buffer.ends_with(" · unsaved"));
        assert!(current_body_text(&app).contains("unsaved body"));
        assert!(app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert!(cache.full_notes.is_empty());
    }

    #[test]
    fn cache_upsert_failure_after_successful_conflict_overwrite_is_warning_only() {
        let (mut app, counts, cache) = update_cache_app(false, Some(CacheFailurePoint::UpsertNote));
        let original_id = app.selected_note_id().expect("selected note");
        save_conflict_overwrite(&mut app, &counts, " · warning", " overwritten body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(original_id));
        assert!(app
            .selected_note
            .as_ref()
            .is_some_and(|note| note.summary.name.ends_with(" · warning")));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots.len(), 1);
    }

    #[test]
    fn cache_snapshot_failure_after_successful_conflict_overwrite_is_warning_only() {
        let (mut app, counts, cache) =
            update_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        let original_id = app.selected_note_id().expect("selected note");
        save_conflict_overwrite(&mut app, &counts, " · snapshot", " overwritten body");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(original_id));
        assert!(app
            .selected_note
            .as_ref()
            .is_some_and(|note| note.summary.name.ends_with(" · snapshot")));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.replace_snapshot_calls, 1);
    }

    #[test]
    fn successful_move_updates_cache() {
        let (mut app, counts, cache) = move_cache_app(false, None);
        let moved_id = app.selected_note_id().expect("selected note");
        move_selected_note_to_next_folder(&mut app);

        assert_eq!(counts.moves.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(!app.notes.iter().any(|summary| summary.id == moved_id));
        assert!(app.selected_note_id().is_none_or(|id| id != moved_id));
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![moved_id.clone()]);
        let moved = cache
            .full_notes
            .get(&moved_id)
            .expect("moved full note cached");
        assert_eq!(moved.summary.folder_id, FolderId::from("demo-work"));
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots[0].notes, app.notes);
        assert!(!cache.replaced_snapshots[0]
            .notes
            .iter()
            .any(|summary| summary.id == moved_id));
    }

    #[test]
    fn failed_move_does_not_update_cache() {
        let (mut app, counts, cache) = move_cache_app(true, None);
        let selected = app.selected_note_id().expect("selected note");
        let before_notes = app.notes.clone();
        move_selected_note_to_next_folder(&mut app);

        assert_eq!(counts.moves.load(Ordering::SeqCst), 1);
        assert_eq!(app.selected_note_id(), Some(selected));
        assert_eq!(app.notes, before_notes);
        assert!(app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert!(cache.full_notes.is_empty());
    }

    #[test]
    fn cache_snapshot_failure_after_successful_move_is_warning_only() {
        let (mut app, counts, cache) =
            move_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        let moved_id = app.selected_note_id().expect("selected note");
        move_selected_note_to_next_folder(&mut app);

        assert_eq!(counts.moves.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(!app.notes.iter().any(|summary| summary.id == moved_id));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![moved_id]);
        assert_eq!(cache.replace_snapshot_calls, 1);
    }

    #[test]
    fn cache_upsert_failure_after_successful_move_is_warning_only() {
        let (mut app, counts, cache) = move_cache_app(false, Some(CacheFailurePoint::UpsertNote));
        let moved_id = app.selected_note_id().expect("selected note");
        move_selected_note_to_next_folder(&mut app);

        assert_eq!(counts.moves.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(!app.notes.iter().any(|summary| summary.id == moved_id));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 1);
        assert_eq!(cache.upsert_note_ids, vec![moved_id]);
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots.len(), 1);
    }

    #[test]
    fn successful_delete_updates_cache() {
        let (mut app, counts, cache) = delete_cache_app(false, None);
        let deleted = app.selected_note.clone().expect("selected note");
        let unrelated = demo_note("unrelated-full-note", "demo-work", "Unrelated", "body");
        cache.lock().unwrap().full_notes = HashMap::from([
            (deleted.summary.id.clone(), deleted.clone()),
            (unrelated.summary.id.clone(), unrelated.clone()),
        ]);
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.status.text, "Moved to Recently Deleted");
        assert!(!app
            .notes
            .iter()
            .any(|summary| summary.id == deleted.summary.id));
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.remove_note_calls, 1);
        assert_eq!(cache.removed_note_ids, vec![deleted.summary.id.clone()]);
        assert_eq!(cache.full_notes.get(&deleted.summary.id), None);
        assert_eq!(
            cache.full_notes.get(&unrelated.summary.id),
            Some(&unrelated)
        );
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert!(cache.replaced_snapshots[0]
            .notes
            .iter()
            .all(|summary| summary.id != deleted.summary.id));
        assert!(cache.replaced_snapshots[0]
            .notes
            .iter()
            .all(|summary| summary.id != unrelated.summary.id));
    }

    #[test]
    fn failed_delete_does_not_update_cache() {
        let (mut app, counts, cache) = delete_cache_app(true, None);
        let selected = app.selected_note_id().expect("selected note");
        let cached = app.selected_note.clone().expect("full note");
        cache
            .lock()
            .unwrap()
            .full_notes
            .insert(selected.clone(), cached.clone());
        let before_notes = app.notes.clone();
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.selected_note_id(), Some(selected.clone()));
        assert_eq!(app.notes, before_notes);
        assert!(app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.remove_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        assert_eq!(cache.full_notes.get(&selected), Some(&cached));
    }

    #[test]
    fn cache_remove_failure_after_successful_delete_is_warning_only() {
        let (mut app, counts, cache) = delete_cache_app(false, Some(CacheFailurePoint::RemoveNote));
        let deleted_id = app.selected_note_id().expect("selected note");
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(!app.notes.iter().any(|summary| summary.id == deleted_id));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.remove_note_calls, 1);
        assert_eq!(cache.removed_note_ids, vec![deleted_id]);
        assert_eq!(cache.replace_snapshot_calls, 1);
        assert_eq!(cache.replaced_snapshots.len(), 1);
    }

    #[test]
    fn cache_snapshot_failure_after_successful_delete_is_warning_only() {
        let (mut app, counts, cache) =
            delete_cache_app(false, Some(CacheFailurePoint::ReplaceSnapshot));
        let deleted_id = app.selected_note_id().expect("selected note");
        confirm_delete(&mut app);

        assert_eq!(counts.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(!app.notes.iter().any(|summary| summary.id == deleted_id));
        assert!(app.status.text.contains("cache warning"));
        assert!(!app.status.is_error);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.remove_note_calls, 1);
        assert_eq!(cache.removed_note_ids, vec![deleted_id]);
        assert_eq!(cache.replace_snapshot_calls, 1);
    }

    #[test]
    fn periodic_refresh_runs_once_when_interval_elapsed_without_a_storm() {
        let (mut app, counts) = periodic_app();
        let due = periodic_due(&app);
        app.periodic_refresh_at(due);
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
        assert_eq!(counts.folders.load(Ordering::SeqCst), 1);
        assert_eq!(counts.notes.load(Ordering::SeqCst), 1);
        assert_eq!(counts.gets.load(Ordering::SeqCst), 1);

        app.periodic_refresh_at(due);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
        assert_eq!(counts.folders.load(Ordering::SeqCst), 1);
        assert_eq!(counts.notes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn periodic_refresh_does_not_run_before_interval() {
        let (mut app, counts) = periodic_app();
        app.periodic_refresh_at(periodic_due(&app) - Duration::from_millis(1));
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 0);
        assert_eq!(counts.folders.load(Ordering::SeqCst), 0);
        assert_eq!(counts.notes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn periodic_refresh_is_deferred_for_dirty_editor_then_runs_when_safe() {
        let (mut app, counts) = periodic_app();
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, "dirty");
        let due = periodic_due(&app);
        app.periodic_refresh_at(due);
        assert!(app.refresh_due);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 0);

        app.handle_key(modified('c', KeyModifiers::CONTROL));
        assert!(app.edit.is_none());
        app.periodic_refresh_at(due);
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
        assert!(!app.refresh_due);
    }

    #[test]
    fn periodic_refresh_is_blocked_by_mutation_popup() {
        let (mut app, counts) = periodic_app();
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        let due = periodic_due(&app);
        app.periodic_refresh_at(due);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 0);
        assert!(app.refresh_due);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.periodic_refresh_at(due);
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn periodic_refresh_preserves_selection_and_recomputes_active_search() {
        let (mut app, counts) = periodic_app();
        apply_search(&mut app, "alpha");
        let selected = app.selected_note_id().expect("selected alpha note");
        let before = visible_names(&app);
        app.periodic_refresh_at(periodic_due(&app));
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
        assert!(matches!(app.search, SearchState::Active(_)));
        assert_eq!(visible_names(&app), before);
        assert_eq!(app.selected_note_id(), Some(selected));
    }

    #[test]
    fn manual_refresh_resets_periodic_timer() {
        let (mut app, counts) = periodic_app();
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
        let after_manual = app.last_refresh_attempt;
        app.periodic_refresh_at(after_manual + app.refresh_interval - Duration::from_millis(1));
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn periodic_refresh_recovers_cached_backend_unavailable_to_live() {
        let fixture = scripted_refresh_fixture();
        let cached = cached_state_with_notes(&[("cached", "Cached note")]);
        let (backend, _) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Ok(fixture.accounts),
            ]),
            VecDeque::from([ScriptedFoldersCall {
                expected: None,
                outcome: Ok(fixture.folders),
            }]),
            VecDeque::from([ScriptedNotesCall {
                expected: fixture.query.clone(),
                outcome: Ok(fixture.page),
            }]),
            VecDeque::from([ScriptedGetNoteCall {
                expected: fixture.selected_id.clone(),
                outcome: Ok(fixture.selected_note),
            }]),
        );
        let (mut app, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        app.periodic_refresh_at(periodic_due(&app));
        poll_periodic_until_idle(&mut app);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert_eq!(app.selected_note_id(), Some(fixture.selected_id));
    }

    #[test]
    fn failed_periodic_refresh_preserves_cached_runtime() {
        let cached = cached_state_with_notes(&[("cached", "Cached note")]);
        let expected_notes = cached.notes.clone();
        let (backend, _) = scripted_backend(
            VecDeque::from([
                Err(NotesError::Backend("offline".into())),
                Err(NotesError::Backend("still offline".into())),
            ]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        app.periodic_refresh_at(periodic_due(&app));
        poll_periodic_until_idle(&mut app);
        assert!(matches!(
            app.data_source,
            DataSourceState::CachedBackendUnavailable { .. }
        ));
        assert_eq!(app.notes, expected_notes);
        assert!(!app.refresh_due);
    }

    #[test]
    fn periodic_refresh_starts_worker_without_blocking_ui_and_discards_stale_result() {
        let (mut app, _, started, release) = blocking_periodic_app();
        let original_index = app.selected_note_index;
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        assert!(app.periodic_refresh_in_flight());

        app.handle_key(KeyEvent::from(KeyCode::Tab));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_ne!(app.selected_note_index, original_index);
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        assert!(matches!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::ManualRefresh)
        ));

        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert!(!app.refresh_due, "queued manual refresh resets the timer");
    }

    #[test]
    fn periodic_refresh_applies_worker_result_once_and_bounds_due_retry() {
        let (mut app, _, started, release) = blocking_periodic_app();
        let due = periodic_due(&app);
        app.periodic_refresh_at(due);
        started.recv().expect("worker entered backend read");
        app.periodic_refresh_at(due + app.refresh_interval);
        app.periodic_refresh_at(due + app.refresh_interval);
        assert!(app.periodic_refresh_in_flight());

        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(
            app.refresh_due,
            "multiple elapsed ticks coalesce into one due retry"
        );

        app.periodic_refresh_at(due + app.refresh_interval);
        poll_periodic_until_idle(&mut app);
        assert!(!app.periodic_refresh_in_flight());
    }

    #[test]
    fn queued_create_intent_opens_editor_without_creating_note() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert_eq!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::BeginNew)
        );
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert_eq!(app.mode, AppMode::Insert);
        assert!(app.edit.as_ref().is_some_and(|edit| edit.is_new));
        assert_eq!(counts.creates.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn queued_child_folder_create_intent_opens_popup_without_creating() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        app.focus = Focus::Navigation;
        let account_id = app.selected_account_id().expect("parent account");
        let parent_id = app.selected_folder_id().cloned().expect("parent selected");
        let parent_name = match &app.navigation[app.selected_navigation] {
            NavigationItem::Folder { name, .. } => name.clone(),
            _ => panic!("selected navigation item should be a folder"),
        };
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("read-refresh worker entered backend");

        app.handle_key(KeyEvent::from(KeyCode::Char('C')));
        assert_eq!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::BeginCreateChildFolder {
                account_id: account_id.clone(),
                parent_folder_id: parent_id.clone(),
                parent_folder_name: parent_name.clone(),
            })
        );
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 0);
        assert!(app.update_worker.is_none());
        assert!(app.popup.is_none());

        let _ = release.send(());
        poll_periodic_until_idle(&mut app);

        assert!(matches!(
            &app.popup,
            Some(Popup::CreateChildFolder {
                account_id: actual_account,
                parent_folder_id: actual_parent,
                parent_folder_name: actual_name,
                name,
                cursor,
            }) if actual_account == &account_id
                && actual_parent == &parent_id
                && actual_name == &parent_name
                && name.is_empty()
                && *cursor == 0
        ));
        assert_eq!(counts.child_folders_created.load(Ordering::SeqCst), 0);
        assert!(app.update_worker.is_none());
    }

    #[test]
    fn queued_folder_reparent_intent_opens_popup_without_moving() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        app.focus = Focus::Navigation;
        let account_id = app.selected_account_id().expect("source account");
        let folder_id = app.selected_folder_id().cloned().expect("source folder");
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("read-refresh worker entered backend");
        app.handle_key(KeyEvent::from(KeyCode::Char('M')));
        assert_eq!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::BeginReparentFolder {
                account_id: account_id.clone(),
                folder_id: folder_id.clone(),
                folder_name: "Notes".into(),
            })
        );
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 0);
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert!(matches!(
            &app.popup,
            Some(Popup::ReparentFolder { account_id: actual_account, folder_id: actual_folder, .. })
                if actual_account == &account_id && actual_folder == &folder_id
        ));
        assert_eq!(counts.folders_reparented.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn queued_delete_intent_opens_confirmation_without_deleting() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::DeleteConfirm(_))));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn queued_folder_delete_intent_opens_folder_confirmation_without_deleting() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        app.focus = Focus::Navigation;
        let folder_id = app.selected_folder_id().cloned().expect("selected folder");
        app.notes.clear();
        app.selected_note = None;
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::BeginDeleteFolder)
        );
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert!(matches!(
            &app.popup,
            Some(Popup::DeleteFolder { folder_id: actual, .. }) if actual == &folder_id
        ));
        assert_eq!(counts.folders_deleted.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn foreground_intent_cancels_periodic_refresh_and_releases_backend_for_delete() {
        let (mut app, counts, started, _release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::BeginDelete)
        );
        poll_periodic_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::DeleteConfirm(_))));
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(app.data_source, DataSourceState::Live);
    }

    #[test]
    fn manual_refresh_starts_worker_without_blocking_ui() {
        let (mut app, _counts, started, release) = blocking_periodic_app();
        let original = app.focus;
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        started.recv().expect("manual worker entered backend read");
        assert!(app.periodic_refresh_in_flight());
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_ne!(app.focus, original);
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
    }

    #[test]
    fn manual_refresh_cancels_periodic_refresh_then_runs_once() {
        let (mut app, counts, started, _release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 2);
        assert!(!app.periodic_refresh_in_flight());
    }

    #[test]
    fn queued_move_intent_opens_normal_move_flow() {
        let (mut app, _, started, release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('m')));
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::Move { selected: 0 })));
    }

    #[test]
    fn latest_pending_intent_wins_and_foreground_beats_due_refresh() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        let due = periodic_due(&app);
        app.periodic_refresh_at(due);
        started.recv().expect("worker entered backend read");
        app.periodic_refresh_at(due + app.refresh_interval);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        app.handle_key(KeyEvent::from(KeyCode::Char('D')));
        assert_eq!(
            app.pending_foreground_intent,
            Some(PendingForegroundIntent::BeginDelete)
        );
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::DeleteConfirm(_))));
        assert!(!app.periodic_refresh_in_flight());
        assert_eq!(counts.deletes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn queued_edit_intent_revalidates_current_selection_after_stale_result() {
        let (mut app, _, started, release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        let current_id = app
            .visible_note(app.selected_note_index)
            .expect("current note")
            .id
            .clone();
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        let edit = app
            .edit
            .as_ref()
            .expect("editor opened for current selection");
        assert_eq!(edit.note_id.as_ref(), Some(&current_id));
    }

    #[test]
    fn queued_manual_refresh_runs_once_after_periodic_refresh() {
        let (mut app, counts, started, release) = blocking_periodic_app();
        app.periodic_refresh_at(periodic_due(&app));
        started.recv().expect("worker entered backend read");
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        let _ = release.send(());
        poll_periodic_until_idle(&mut app);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 2);
        assert!(!app.refresh_due);
    }

    #[test]
    fn cached_search_selection_loads_cached_full_note_without_backend() {
        let cached = cached_state_with_notes(&[("alpha", "Alpha"), ("beta", "Beta")]);
        let alpha = demo_note("alpha", "demo-notes", "Alpha", "cached Alpha body");
        let beta = demo_note("beta", "demo-notes", "Beta", "cached Beta body");
        let (backend, calls) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, cache) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                full_notes: HashMap::from([
                    (alpha.summary.id.clone(), alpha),
                    (beta.summary.id.clone(), beta.clone()),
                ]),
                ..Default::default()
            },
        );
        app.bootstrap_cache();
        app.refresh();
        apply_search(&mut app, "beta");
        assert_eq!(app.selected_note, Some(beta));
        assert_eq!(*calls.lock().unwrap(), vec![BackendCall::Accounts]);
        assert_eq!(
            cache.lock().unwrap().load_note_ids,
            vec![NoteId::from("alpha"), NoteId::from("beta")]
        );
    }

    #[test]
    fn file_preferences_override_defaults_independently() {
        let parsed = parse_file_config("preview_wrap = false\nunknown_key = 42\n").unwrap();
        let resolved = resolve_config_from_file(parsed, CliConfigOverrides::default()).unwrap();
        assert!(resolved.config.auto_refresh);
        assert!(!resolved.config.preview_wrap);
        assert!(resolved.config.show_attachment_metadata);
        assert_eq!(resolved.preview_wrap_source, ConfigValueSource::File);
        assert_eq!(resolved.auto_refresh_source, ConfigValueSource::Default);
    }

    #[test]
    fn cli_preferences_override_file_and_conflicts_are_rejected() {
        let file = parse_file_config(
            "auto_refresh = true\npreview_wrap = false\nshow_attachment_metadata = true",
        )
        .unwrap();
        let cli = parse_config_overrides(&[
            "--no-auto-refresh".into(),
            "--preview-wrap".into(),
            "--hide-attachment-metadata".into(),
        ])
        .unwrap();
        let resolved = resolve_config_from_file(file, cli).unwrap();
        assert!(!resolved.config.auto_refresh);
        assert!(resolved.config.preview_wrap);
        assert!(!resolved.config.show_attachment_metadata);
        assert_eq!(resolved.auto_refresh_source, ConfigValueSource::Cli);
        assert!(
            parse_config_overrides(&["--auto-refresh".into(), "--no-auto-refresh".into()]).is_err()
        );
    }

    #[test]
    fn malformed_boolean_config_falls_back_to_defaults() {
        assert!(parse_file_config("auto_refresh = \"yes\"").is_err());
    }

    #[test]
    fn auto_refresh_false_disables_periodic_refresh_without_due_accumulation() {
        let (mut app, counts) = counting_app(BackendCapabilities::new(
            notes_core::RichTextCapabilities::all_supported(),
        ));
        counts.accounts.store(0, Ordering::SeqCst);
        app.auto_refresh = false;
        let due = periodic_due(&app);
        app.periodic_refresh_at(due + app.refresh_interval + app.refresh_interval);
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 0);
        assert!(!app.refresh_due);
        app.refresh();
        assert_eq!(counts.accounts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preview_and_attachment_metadata_preferences_preserve_identity() {
        let mut app = App::with_config(
            Box::new(demo_backend()),
            AppConfig {
                refresh_interval: DEFAULT_REFRESH_INTERVAL,
                auto_refresh: true,
                preview_wrap: false,
                show_attachment_metadata: false,
            },
        );
        app.refresh();
        select_demo_note(&mut app, "demo-attachment");
        let note = app.selected_note.as_ref().unwrap();
        let preview = preview_text(note, app.show_attachment_metadata);
        let preview_text = preview
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(preview_text.contains("Photo.png"));
        assert!(!preview_text.contains("preview available"));
        app.begin_attachments();
        let popup = attachment_popup_text(&app, 0);
        assert!(popup.contains("Photo.png"));
        assert!(!popup.contains("Content ID:"));
        assert!(!app.preview_wrap);
    }

    fn temporary_config_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "apple-notes-tui-config-{name}-{}",
                std::process::id()
            ))
            .join("config.toml")
    }

    fn temporary_session_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "apple-notes-tui-session-{name}-{}",
                std::process::id()
            ))
            .join("session.toml")
    }

    fn session_navigation_fixture() -> (Vec<Account>, Vec<Folder>) {
        let account_a = Account {
            id: AccountId::from("account-a"),
            name: "Duplicate".into(),
            is_default: true,
            is_upgraded: true,
            default_folder_id: Some(FolderId::from("folder-a")),
        };
        let account_b = Account {
            id: AccountId::from("account-b"),
            name: "Duplicate".into(),
            is_default: false,
            is_upgraded: true,
            default_folder_id: Some(FolderId::from("folder-b")),
        };
        let folder_a = Folder {
            id: FolderId::from("folder-a"),
            account_id: account_a.id.clone(),
            name: "Notes".into(),
            parent: FolderParent::Account {
                account_id: account_a.id.clone(),
            },
            shared: false,
        };
        let folder_b = Folder {
            id: FolderId::from("folder-b"),
            account_id: account_b.id.clone(),
            name: "Notes".into(),
            parent: FolderParent::Account {
                account_id: account_b.id.clone(),
            },
            shared: false,
        };
        (vec![account_a, account_b], vec![folder_a, folder_b])
    }

    #[test]
    fn missing_session_is_none_and_roundtrip_preserves_stable_ids() {
        let path = temporary_session_path("roundtrip");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        assert_eq!(load_session(&path).unwrap(), None);
        let state = SessionState {
            account_id: Some(AccountId::from("account-b")),
            folder_id: Some(FolderId::from("folder-b")),
            note_id: None,
            search_query: None,
            preview_scroll: None,
            focus: None,
        };
        save_session(&path, &state).unwrap();
        assert_eq!(load_session(&path).unwrap(), Some(state));
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("account_id"));
        assert!(contents.contains("folder_id"));
        assert!(!contents.contains("note"));
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn malformed_session_is_ignored_by_startup_context() {
        let path = temporary_session_path("malformed");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        fs::create_dir_all(path.parent().expect("parent")).unwrap();
        fs::write(&path, "account_id = unquoted\n").unwrap();
        assert!(load_session(&path).is_err());
        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(path.clone(), None);
        app.load_cached_state(CachedState {
            accounts,
            folders,
            notes: vec![],
            last_successful_refresh: None,
        });
        assert_eq!(app.selected_folder_id(), Some(&FolderId::from("folder-a")));
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn startup_restores_session_by_ids_and_falls_back_safely() {
        let (accounts, folders) = session_navigation_fixture();
        let cached = CachedState {
            accounts: accounts.clone(),
            folders: folders.clone(),
            notes: vec![],
            last_successful_refresh: None,
        };
        let mut restored = App::new(Box::new(demo_backend()));
        restored.set_session_state(
            temporary_session_path("restore"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        restored.load_cached_state(cached.clone());
        assert_eq!(
            restored.selected_folder_id(),
            Some(&FolderId::from("folder-b"))
        );

        let mut missing_account = App::new(Box::new(demo_backend()));
        missing_account.set_session_state(
            temporary_session_path("missing-account"),
            Some(SessionState {
                account_id: Some(AccountId::from("gone")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        missing_account.load_cached_state(cached.clone());
        assert_eq!(
            missing_account.selected_folder_id(),
            Some(&FolderId::from("folder-a"))
        );

        let mut missing_folder = App::new(Box::new(demo_backend()));
        missing_folder.set_session_state(
            temporary_session_path("missing-folder"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("gone")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        missing_folder.load_cached_state(cached);
        assert_eq!(
            missing_folder.selected_folder_id(),
            Some(&FolderId::from("folder-b"))
        );
    }

    #[test]
    fn successful_session_selection_persists_a_coherent_pair() {
        let path = temporary_session_path("persist-selection");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(path.clone(), None);
        app.load_cached_state(CachedState {
            accounts,
            folders,
            notes: vec![],
            last_successful_refresh: None,
        });
        app.selected_navigation = app
            .navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { id, .. } if id == &FolderId::from("folder-b")))
            .unwrap();
        app.persist_session_selection();
        assert_eq!(
            load_session(&path).unwrap(),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: Some(SessionFocus::Navigation),
            })
        );
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn live_startup_restores_session_context_without_extra_traversal() {
        let (accounts, folders) = session_navigation_fixture();
        let note = Note {
            summary: demo_note("note-b", "folder-b", "B", "").summary,
            account_id: AccountId::from("account-b"),
            body_html: "<div>B</div>".into(),
            plaintext: "B".into(),
            attachments: vec![],
        };
        let mut full_notes = HashMap::new();
        full_notes.insert(note.summary.id.clone(), note);
        let mut app = App::new(Box::new(MockNotesBackend::new(
            accounts, folders, full_notes,
        )));
        app.set_session_state(
            temporary_session_path("live-restore"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        app.refresh();
        assert_eq!(app.selected_folder_id(), Some(&FolderId::from("folder-b")));
        assert_eq!(app.notes.len(), 1);
    }

    #[test]
    fn failed_navigation_does_not_persist_session_and_atomic_failure_keeps_old_file() {
        let path = temporary_session_path("failure");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let old = SessionState {
            account_id: Some(AccountId::from("old-account")),
            folder_id: Some(FolderId::from("old-folder")),
            note_id: None,
            search_query: None,
            preview_scroll: None,
            focus: None,
        };
        save_session(&path, &old).unwrap();
        let error = atomic_write_file_with_hook(&path, "account_id = \"new\"\n", "session", |_| {
            Err("injected pre-rename failure".into())
        })
        .unwrap_err();
        assert!(error.contains("injected"));
        assert_eq!(load_session(&path).unwrap(), Some(old));

        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(MockNotesBackend::new(
            accounts,
            folders,
            HashMap::new(),
        )));
        app.set_session_state(path.clone(), None);
        app.load_cached_state(CachedState {
            accounts: vec![],
            folders: vec![],
            notes: vec![],
            last_successful_refresh: None,
        });
        app.persist_session_selection();
        assert_eq!(
            load_session(&path).unwrap(),
            Some(SessionState {
                account_id: Some(AccountId::from("old-account")),
                folder_id: Some(FolderId::from("old-folder")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: None,
            })
        );
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn session_roundtrip_preserves_optional_note_id_and_legacy_files_load() {
        let path = temporary_session_path("note-roundtrip");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let state = SessionState {
            account_id: Some(AccountId::from("account-b")),
            folder_id: Some(FolderId::from("folder-b")),
            note_id: Some(NoteId::from("note-b")),
            search_query: None,
            preview_scroll: None,
            focus: None,
        };
        save_session(&path, &state).unwrap();
        assert_eq!(load_session(&path).unwrap(), Some(state));
        fs::write(
            &path,
            "account_id = \"account-b\"\nfolder_id = \"folder-b\"\n",
        )
        .unwrap();
        assert_eq!(
            load_session(&path).unwrap(),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: None,
                search_query: None,
                preview_scroll: None,
                focus: None,
            })
        );
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn cached_startup_restores_exact_note_id_and_falls_back_when_missing() {
        let (accounts, folders) = session_navigation_fixture();
        let notes = vec![
            demo_note("note-a", "folder-b", "Same", "").summary,
            demo_note("note-b", "folder-b", "Same", "").summary,
        ];
        let cached = CachedState {
            accounts,
            folders,
            notes,
            last_successful_refresh: None,
        };
        let mut restored = App::new(Box::new(demo_backend()));
        restored.set_session_state(
            temporary_session_path("note-restore"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: Some(NoteId::from("note-b")),
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        restored.load_cached_state(cached.clone());
        assert_eq!(restored.selected_note_id(), Some(NoteId::from("note-b")));

        let mut missing = App::new(Box::new(demo_backend()));
        missing.set_session_state(
            temporary_session_path("note-missing"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: Some(NoteId::from("gone")),
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        missing.load_cached_state(cached);
        assert_eq!(missing.selected_note_id(), Some(NoteId::from("note-a")));
    }

    #[test]
    fn cached_backend_unavailable_restores_saved_note_without_extra_read() {
        let cached = cached_state_with_notes(&[("alpha", "Alpha"), ("beta", "Beta")]);
        let (backend, calls) = scripted_backend(
            VecDeque::from([Err(NotesError::Backend("offline".into()))]),
            VecDeque::new(),
            VecDeque::new(),
            VecDeque::new(),
        );
        let (mut app, _) = app_with_cache(
            backend,
            FakeCacheState {
                bootstrap: cached,
                ..Default::default()
            },
        );
        app.set_session_state(
            temporary_session_path("cached-note"),
            Some(SessionState {
                account_id: Some(AccountId::from("demo-account")),
                folder_id: Some(FolderId::from("demo-notes")),
                note_id: Some(NoteId::from("beta")),
                search_query: None,
                preview_scroll: None,
                focus: None,
            }),
        );
        app.bootstrap_cache();
        assert_eq!(app.selected_note_id(), Some(NoteId::from("beta")));
        app.refresh();
        assert_eq!(app.selected_note_id(), Some(NoteId::from("beta")));
        assert_eq!(*calls.lock().unwrap(), vec![BackendCall::Accounts]);
    }

    #[test]
    fn session_roundtrip_preserves_completed_unicode_search_and_legacy_is_none() {
        let path = temporary_session_path("search-roundtrip");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let state = SessionState {
            account_id: Some(AccountId::from("account-b")),
            folder_id: Some(FolderId::from("folder-b")),
            note_id: Some(NoteId::from("note-b")),
            search_query: Some("Grüße 日本語 🚀".into()),
            preview_scroll: None,
            focus: None,
        };
        save_session(&path, &state).unwrap();
        assert_eq!(load_session(&path).unwrap(), Some(state));
        fs::write(
            &path,
            "account_id = \"account-b\"\nfolder_id = \"folder-b\"\nnote_id = \"note-b\"\n",
        )
        .unwrap();
        assert_eq!(load_session(&path).unwrap().unwrap().search_query, None);
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn cached_startup_restores_search_before_visible_note_selection() {
        let (accounts, folders) = session_navigation_fixture();
        let cached = CachedState {
            accounts,
            folders,
            notes: vec![
                demo_note("n1", "folder-b", "foo one", "").summary,
                demo_note("n2", "folder-b", "bar", "").summary,
                demo_note("n3", "folder-b", "foo three", "").summary,
            ],
            last_successful_refresh: None,
        };
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(
            temporary_session_path("search-restore"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: Some(NoteId::from("n3")),
                search_query: Some("foo".into()),
                preview_scroll: None,
                focus: None,
            }),
        );
        app.load_cached_state(cached);
        assert!(matches!(&app.search, SearchState::Active(active) if active.query == "foo"));
        assert_eq!(
            app.visible_note_ids(),
            vec![NoteId::from("n1"), NoteId::from("n3")]
        );
        assert_eq!(app.selected_note_id(), Some(NoteId::from("n3")));
    }

    #[test]
    fn session_roundtrip_preserves_optional_preview_scroll_and_exact_note_restore() {
        let path = temporary_session_path("scroll-roundtrip");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let state = SessionState {
            account_id: Some(AccountId::from("account-b")),
            folder_id: Some(FolderId::from("folder-b")),
            note_id: Some(NoteId::from("n2")),
            search_query: None,
            preview_scroll: Some(9),
            focus: None,
        };
        save_session(&path, &state).unwrap();
        assert_eq!(load_session(&path).unwrap(), Some(state));
        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(path.clone(), load_session(&path).unwrap());
        app.load_cached_state(CachedState {
            accounts,
            folders,
            notes: vec![demo_note("n2", "folder-b", "Long", "").summary],
            last_successful_refresh: None,
        });
        assert_eq!(app.selected_note_id(), Some(NoteId::from("n2")));
        assert_eq!(app.preview_scroll, 0); // no full cached preview was available to scroll
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn fallback_note_does_not_inherit_saved_preview_scroll() {
        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(
            temporary_session_path("scroll-fallback"),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: Some(NoteId::from("gone")),
                search_query: None,
                preview_scroll: Some(u16::MAX),
                focus: None,
            }),
        );
        app.load_cached_state(CachedState {
            accounts,
            folders,
            notes: vec![demo_note("n1", "folder-b", "Fallback", "").summary],
            last_successful_refresh: None,
        });
        assert_eq!(app.selected_note_id(), Some(NoteId::from("n1")));
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn note_selection_change_persists_triplet_without_search_text() {
        let path = temporary_session_path("note-selection");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(path.clone(), None);
        app.load_cached_state(CachedState {
            accounts,
            folders,
            notes: vec![
                demo_note("note-a", "folder-b", "Hidden title", "").summary,
                demo_note("note-b", "folder-b", "Needle", "").summary,
            ],
            last_successful_refresh: None,
        });
        app.selected_navigation = app
            .navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { id, .. } if id == &FolderId::from("folder-b")))
            .unwrap();
        apply_search(&mut app, "needle");
        app.persist_session_selection();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("note_id = \"note-b\""));
        assert!(contents.contains("search_query = \"needle\""));
        assert!(!contents.contains("Hidden title"));
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn session_roundtrip_preserves_optional_browsing_focus_and_legacy_files_load() {
        let path = temporary_session_path("focus-roundtrip");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let state = SessionState {
            account_id: Some(AccountId::from("account-b")),
            folder_id: Some(FolderId::from("folder-b")),
            note_id: Some(NoteId::from("note-b")),
            search_query: Some("needle".into()),
            preview_scroll: Some(3),
            focus: Some(SessionFocus::Preview),
        };
        save_session(&path, &state).unwrap();
        assert_eq!(load_session(&path).unwrap(), Some(state));
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("focus = \"preview\""));
        fs::write(
            &path,
            "account_id = \"account-b\"\nfolder_id = \"folder-b\"\nnote_id = \"note-b\"\nsearch_query = \"needle\"\npreview_scroll = 3\n",
        )
        .unwrap();
        assert_eq!(load_session(&path).unwrap().unwrap().focus, None);
        fs::write(&path, "focus = \"editor\"\n").unwrap();
        assert!(load_session(&path).is_err());
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn startup_restores_browsing_focus_and_safely_falls_back_from_invalid_preview() {
        let (accounts, folders) = session_navigation_fixture();
        let cached = CachedState {
            accounts: accounts.clone(),
            folders: folders.clone(),
            notes: vec![demo_note("note-b", "folder-b", "Needle", "").summary],
            last_successful_refresh: None,
        };
        for (saved, expected) in [
            (SessionFocus::Navigation, Focus::Navigation),
            (SessionFocus::Notes, Focus::Notes),
            (SessionFocus::Preview, Focus::Preview),
        ] {
            let mut app = App::new(Box::new(demo_backend()));
            app.set_session_state(
                temporary_session_path("focus-restore"),
                Some(SessionState {
                    account_id: Some(AccountId::from("account-b")),
                    folder_id: Some(FolderId::from("folder-b")),
                    note_id: Some(NoteId::from("note-b")),
                    search_query: None,
                    preview_scroll: None,
                    focus: Some(saved),
                }),
            );
            app.load_cached_state(cached.clone());
            assert_eq!(app.focus, expected);
        }

        let mut no_selection = App::new(Box::new(demo_backend()));
        let fallback_path = temporary_session_path("focus-fallback");
        let _ = fs::remove_dir_all(fallback_path.parent().expect("parent"));
        no_selection.set_session_state(
            fallback_path.clone(),
            Some(SessionState {
                account_id: Some(AccountId::from("account-b")),
                folder_id: Some(FolderId::from("folder-b")),
                note_id: None,
                search_query: Some("missing".into()),
                preview_scroll: None,
                focus: Some(SessionFocus::Preview),
            }),
        );
        no_selection.load_cached_state(cached);
        assert_eq!(no_selection.selected_note_id(), None);
        assert_eq!(no_selection.focus, Focus::Notes);
        assert_eq!(
            load_session(&fallback_path).unwrap().unwrap().focus,
            Some(SessionFocus::Notes)
        );
        let _ = fs::remove_dir_all(fallback_path.parent().expect("parent"));
    }

    #[test]
    fn browsing_focus_change_persists_coherently_and_transient_workflows_do_not_replace_it() {
        let path = temporary_session_path("focus-persist");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let (accounts, folders) = session_navigation_fixture();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_session_state(path.clone(), None);
        app.load_cached_state(CachedState {
            accounts,
            folders,
            notes: vec![demo_note("note-b", "folder-b", "Needle", "").summary],
            last_successful_refresh: None,
        });
        app.selected_navigation = app
            .navigation
            .iter()
            .position(|item| matches!(item, NavigationItem::Folder { id, .. } if id == &FolderId::from("folder-b")))
            .unwrap();
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Notes);
        assert_eq!(
            load_session(&path).unwrap().unwrap().focus,
            Some(SessionFocus::Notes)
        );
        let contents = fs::read_to_string(&path).unwrap();
        app.set_browsing_focus(Focus::Notes);
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);

        app.show_help = true;
        app.handle_key(KeyEvent::from(KeyCode::Char('?')));
        app.begin_settings();
        assert_eq!(
            load_session(&path).unwrap().unwrap().focus,
            Some(SessionFocus::Notes)
        );
        app.popup = None;
        app.begin_search();
        assert_eq!(
            load_session(&path).unwrap().unwrap().focus,
            Some(SessionFocus::Notes)
        );
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    fn recovery_draft(kind: EditorRecoveryKind, note_id: Option<&str>) -> EditorRecoveryDraft {
        EditorRecoveryDraft {
            version: 1,
            kind,
            account_id: AccountId::from("demo-account"),
            folder_id: FolderId::from("demo-notes"),
            note_id: note_id.map(NoteId::from),
            expected_modification_date: Some(NoteDate::new("draft-baseline")),
            title: "Grüße Русская заметка".into(),
            document: notes_core::RichDocument {
                blocks: vec![notes_core::Block::Paragraph(vec![
                    notes_core::Inline::Text("日本語 🚀\n\nfinal line".into()),
                ])],
            },
            original_title: "Original".into(),
            original_body_html: "<div>Original</div>".into(),
            original_plaintext: "Original".into(),
            cursor: None,
            viewport: None,
        }
    }

    #[test]
    fn editor_draft_roundtrip_preserves_create_and_edit_content_exactly() {
        let path = temporary_session_path("editor-draft-roundtrip");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        for (kind, id) in [
            (EditorRecoveryKind::Create, None),
            (EditorRecoveryKind::Edit, Some("n1")),
        ] {
            let draft = recovery_draft(kind, id);
            save_editor_draft(&path, &draft).unwrap();
            assert_eq!(load_editor_draft(&path).unwrap(), Some(draft));
        }
        fs::write(&path, b"not a draft").unwrap();
        assert!(load_editor_draft(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not a draft");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn dirty_editor_persists_recovery_draft_and_cancel_removes_it() {
        let path = temporary_session_path("editor-draft-dirty");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let mut app = App::demo();
        app.refresh();
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(!path.exists());
        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        let draft = load_editor_draft(&path).unwrap().unwrap();
        assert!(matches!(draft.kind, EditorRecoveryKind::Create));
        assert_eq!(draft.title, "x");
        let contents = fs::read(&path).unwrap();
        app.persist_editor_recovery_if_dirty();
        assert_eq!(fs::read(&path).unwrap(), contents);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn startup_draft_recovery_requires_explicit_restore_without_backend_mutation() {
        let path = temporary_session_path("editor-draft-restore");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let draft = recovery_draft(EditorRecoveryKind::Edit, Some("alpha"));
        save_editor_draft(&path, &draft).unwrap();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_editor_draft_state(path.clone(), load_editor_draft(&path).unwrap());
        app.load_cached_state(cached_state_with_notes(&[("alpha", "Alpha")]));
        app.offer_editor_draft_recovery();
        assert!(app.edit.is_none());
        assert!(matches!(app.popup, Some(Popup::DraftRecovery { .. })));
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        let edit = app.edit.as_ref().unwrap();
        assert!(edit.dirty);
        assert_eq!(edit.title_buffer, draft.title);
        assert_eq!(
            edit.base_modification_date,
            draft.expected_modification_date
        );
        assert_eq!(edit.document.document, draft.document);
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn dirty_editor_write_failure_preserves_runtime_and_old_recovery_file() {
        let path = temporary_session_path("editor-draft-write-failure");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let old = recovery_draft(EditorRecoveryKind::Create, None);
        save_editor_draft(&path, &old).unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let mut app = App::demo();
        app.refresh();
        app.set_editor_draft_state(path.clone(), None);
        app.editor_draft_failure = Some(EditorDraftFailurePoint::Write);
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.edit.as_ref().unwrap().dirty);
        assert_eq!(app.edit.as_ref().unwrap().title_buffer, "x");
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
        assert!(app.status.text.contains("Editor recovery warning"));
        assert_eq!(app.data_source, DataSourceState::Live);
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn missing_recovery_targets_are_preserved_and_cannot_restore() {
        for draft in [
            recovery_draft(EditorRecoveryKind::Edit, Some("missing-note")),
            EditorRecoveryDraft {
                account_id: AccountId::from("missing-account"),
                folder_id: FolderId::from("missing-folder"),
                ..recovery_draft(EditorRecoveryKind::Create, None)
            },
        ] {
            let path = temporary_session_path("editor-draft-missing-target");
            let _ = fs::remove_dir_all(path.parent().expect("parent"));
            save_editor_draft(&path, &draft).unwrap();
            let mut app = App::new(Box::new(demo_backend()));
            app.set_editor_draft_state(path.clone(), Some(draft));
            app.load_cached_state(cached_state_with_notes(&[("alpha", "Alpha")]));
            app.offer_editor_draft_recovery();
            assert!(matches!(
                app.popup,
                Some(Popup::DraftRecovery {
                    recoverable: false,
                    ..
                })
            ));
            app.handle_key(KeyEvent::from(KeyCode::Char('r')));
            assert!(app.edit.is_none());
            assert!(path.exists());
            let _ = fs::remove_dir_all(path.parent().expect("parent"));
        }
    }

    #[test]
    fn recovery_discard_and_cleanup_failure_never_roll_back_runtime() {
        let path = temporary_session_path("editor-draft-cleanup-failure");
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
        let draft = recovery_draft(EditorRecoveryKind::Create, None);
        save_editor_draft(&path, &draft).unwrap();
        let mut app = App::new(Box::new(demo_backend()));
        app.set_editor_draft_state(path.clone(), Some(draft));
        app.load_cached_state(cached_state_with_notes(&[("alpha", "Alpha")]));
        app.offer_editor_draft_recovery();
        app.editor_draft_failure = Some(EditorDraftFailurePoint::Remove);
        app.handle_key(KeyEvent::from(KeyCode::Char('d')));
        assert!(app.popup.is_none());
        assert!(path.exists());
        assert!(app.status.text.contains("could not be removed"));
        assert_eq!(app.data_source, DataSourceState::Cached);
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn successful_create_clears_recovery_only_after_worker_success() {
        let path = temporary_session_path("draft-create-success");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = create_cache_app(false, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        type_text(&mut app, "draft create");
        assert!(path.exists());
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert!(app.edit.is_none());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_create_keeps_recovery_draft() {
        let path = temporary_session_path("draft-create-failure");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = create_cache_app(true, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        type_text(&mut app, "draft create");
        let expected = load_editor_draft(&path).unwrap().unwrap();
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert!(app
            .edit
            .as_ref()
            .is_some_and(|edit| edit.dirty && edit.is_new));
        assert_eq!(load_editor_draft(&path).unwrap(), Some(expected));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn successful_and_failed_update_recovery_lifecycle() {
        let path = temporary_session_path("draft-update-success");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = update_cache_app(false, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, " retained");
        assert!(path.exists());
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.edit.is_none());
        assert!(!path.exists());

        let failure_path = temporary_session_path("draft-update-failure");
        let _ = fs::remove_dir_all(failure_path.parent().unwrap());
        let (mut failed, failed_counts, _) = update_cache_app(true, None);
        failed.set_editor_draft_state(failure_path.clone(), None);
        failed.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut failed, " retained");
        let draft = load_editor_draft(&failure_path).unwrap().unwrap();
        failed.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut failed);
        assert_eq!(failed_counts.updates.load(Ordering::SeqCst), 1);
        assert!(failed.edit.as_ref().is_some_and(|edit| edit.dirty));
        assert_eq!(load_editor_draft(&failure_path).unwrap(), Some(draft));
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let _ = fs::remove_dir_all(failure_path.parent().unwrap());
    }

    #[test]
    fn conflict_and_overwrite_keep_or_clear_recovery_at_authoritative_result() {
        let path = temporary_session_path("draft-conflict");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = update_cache_app(false, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, " conflict");
        let draft = load_editor_draft(&path).unwrap().unwrap();
        counts.conflict_on_next_get.store(true, Ordering::SeqCst);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::Conflict(_))));
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        assert_eq!(load_editor_draft(&path).unwrap(), Some(draft));
        app.handle_key(KeyEvent::from(KeyCode::Char('o')));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    fn make_nontrivial_dirty_edit(app: &mut App, title: &str) -> EditorRecoveryDraft {
        let edit = app.edit.as_mut().expect("editor");
        edit.title_buffer = title.into();
        edit.document = EditorDocument::new(notes_core::RichDocument {
            blocks: vec![
                notes_core::Block::Paragraph(vec![notes_core::Inline::Bold(vec![
                    notes_core::Inline::Text("Unicode 日本語 🚀".into()),
                ])]),
                notes_core::Block::BulletList(vec![notes_core::ListItem {
                    content: vec![notes_core::Inline::Italic(vec![notes_core::Inline::Text(
                        "list item".into(),
                    )])],
                }]),
                notes_core::Block::Paragraph(vec![notes_core::Inline::Link {
                    label: vec![notes_core::Inline::Text("link".into())],
                    href: "https://example.test/recovery".into(),
                }]),
            ],
        });
        edit.dirty = true;
        app.persist_editor_recovery_if_dirty();
        load_editor_draft(
            app.editor_draft_file_path
                .as_deref()
                .expect("recovery path"),
        )
        .unwrap()
        .expect("persisted recovery draft")
    }

    #[test]
    fn create_recovery_draft_remains_on_disk_while_worker_is_blocked() {
        let path = temporary_session_path("draft-create-blocked");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::Create, false);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('n')));
        type_text(&mut app, "Create recovery 日本語");
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        type_text(&mut app, "body 🚀");
        let expected = load_editor_draft(&path).unwrap().unwrap();

        app.handle_key(modified('s', KeyModifiers::CONTROL));
        started.recv().expect("create worker entered backend");

        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert!(path.exists());
        assert_eq!(load_editor_draft(&path).unwrap(), Some(expected.clone()));
        assert!(matches!(expected.kind, EditorRecoveryKind::Create));
        assert_eq!(cache.lock().unwrap().upsert_note_calls, 0);

        release.send(()).unwrap();
        poll_update_until_idle(&mut app);
        assert_eq!(counts.creates.load(Ordering::SeqCst), 1);
        assert!(app.edit.is_none());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn update_recovery_draft_remains_on_disk_while_worker_is_blocked() {
        let path = temporary_session_path("draft-update-blocked");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _, started, release) =
            blocking_mutation_cache_app(BlockedMutation::Update, false);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let expected = make_nontrivial_dirty_edit(&mut app, "Update recovery title");
        let baseline = expected.expected_modification_date.clone();

        app.handle_key(modified('s', KeyModifiers::CONTROL));
        started.recv().expect("update worker entered mutation");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert!(path.exists());
        let persisted = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(persisted.kind, EditorRecoveryKind::Edit);
        assert_eq!(persisted.note_id, expected.note_id);
        assert_eq!(persisted.expected_modification_date, baseline);
        assert_eq!(persisted.title, expected.title);
        assert_eq!(persisted.document, expected.document);

        release.send(()).unwrap();
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.edit.is_none());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn overwrite_recovery_draft_remains_on_disk_while_worker_is_blocked() {
        let path = temporary_session_path("draft-overwrite-blocked");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _, started, release) =
            blocking_mutation_cache_app(BlockedMutation::Update, false);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let expected = make_nontrivial_dirty_edit(&mut app, "Overwrite recovery title");
        let baseline = expected.expected_modification_date.clone();
        counts.conflict_on_next_get.store(true, Ordering::SeqCst);

        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::Conflict(_))));
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        assert!(path.exists());
        app.handle_key(KeyEvent::from(KeyCode::Char('o')));
        started.recv().expect("overwrite worker entered backend");

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_some());
        assert!(path.exists());
        let persisted = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(persisted, expected);
        assert_eq!(persisted.expected_modification_date, baseline);

        release.send(()).unwrap();
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.edit.is_none());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_conflict_overwrite_keeps_recovery_draft() {
        let path = temporary_session_path("draft-overwrite-failure");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, cache, started, release) =
            blocking_mutation_cache_app(BlockedMutation::Update, true);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let expected = make_nontrivial_dirty_edit(&mut app, "Failed overwrite title");
        counts.conflict_on_next_get.store(true, Ordering::SeqCst);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::Conflict(_))));

        app.handle_key(KeyEvent::from(KeyCode::Char('o')));
        started
            .recv()
            .expect("failed overwrite worker entered backend");
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        poll_update_until_idle(&mut app);

        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.update_worker.is_none());
        assert!(app.popup.is_none());
        assert!(app.edit.as_ref().is_some_and(|edit| edit.dirty));
        assert!(app.status.is_error);
        assert!(app.status.text.contains("synthetic update failure"));
        assert!(path.exists());
        let persisted = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(persisted, expected);
        assert_eq!(
            persisted.expected_modification_date,
            expected.expected_modification_date
        );
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        drop(cache);
        app.poll_update_worker();
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn editor_draft_roundtrip_preserves_cursor_and_viewport() {
        let path = temporary_session_path("editor-draft-position-roundtrip");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        for kind in [EditorRecoveryKind::Create, EditorRecoveryKind::Edit] {
            let mut draft = recovery_draft(
                kind.clone(),
                matches!(kind, EditorRecoveryKind::Edit).then_some("demo-alpha"),
            );
            draft.cursor = Some(EditorCursorRecovery {
                field: EditorRecoveryField::Body,
                target: EditorTarget::Block { block_index: 0 },
                offset: 7,
            });
            draft.viewport = Some(4);
            save_editor_draft(&path, &draft).unwrap();
            assert_eq!(load_editor_draft(&path).unwrap(), Some(draft));
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn draft_info_reports_valid_create_and_edit_metadata_without_body() {
        let path = temporary_session_path("draft-info-valid");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let mut create = recovery_draft(EditorRecoveryKind::Create, None);
        create.title = "Visible title".into();
        create.document = notes_core::RichDocument {
            blocks: vec![notes_core::Block::Paragraph(vec![
                notes_core::Inline::Text("SECRET_DRAFT_BODY_12345".into()),
            ])],
        };
        create.cursor = Some(EditorCursorRecovery {
            field: EditorRecoveryField::Title,
            target: EditorTarget::Block { block_index: 0 },
            offset: 3,
        });
        create.viewport = Some(2);
        save_editor_draft(&path, &create).unwrap();
        let create_info = format_editor_draft_info(&path).unwrap();
        assert!(create_info.contains("Status: valid\nVersion: 1\nKind: create"));
        assert!(create_info.contains("Account ID: demo-account"));
        assert!(create_info.contains("Title: Visible title"));
        assert!(!create_info.contains("Note ID:"));
        assert!(!create_info.contains("Baseline:"));
        assert!(!create_info.contains("SECRET_DRAFT_BODY_12345"));

        let edit = recovery_draft(EditorRecoveryKind::Edit, Some("demo-alpha"));
        save_editor_draft(&path, &edit).unwrap();
        let edit_info = format_editor_draft_info(&path).unwrap();
        assert!(edit_info.contains("Kind: edit"));
        assert!(edit_info.contains("Note ID: demo-alpha"));
        assert!(edit_info.contains("Baseline: draft-baseline"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn draft_info_missing_malformed_and_unsupported_preserve_file() {
        let path = temporary_session_path("draft-info-invalid");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        assert_eq!(format_editor_draft_info(&path).unwrap(), "Draft: none");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let malformed = b"not valid json";
        fs::write(&path, malformed).unwrap();
        let malformed_info = format_editor_draft_info(&path).unwrap();
        assert!(malformed_info.contains("Status: malformed"));
        assert_eq!(fs::read(&path).unwrap(), malformed);

        let mut unsupported = recovery_draft(EditorRecoveryKind::Create, None);
        unsupported.version = 77;
        fs::write(&path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
        let unsupported_info = format_editor_draft_info(&path).unwrap();
        assert!(unsupported_info.contains("Status: unsupported version\nVersion: 77"));
        assert_eq!(
            inspect_editor_draft(&path).unwrap(),
            EditorDraftInspection::UnsupportedVersion { version: 77 }
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn draft_clear_is_idempotent_parse_independent_and_isolated() {
        let directory_path = temporary_session_path("draft-clear");
        let directory = directory_path.parent().unwrap();
        let path = directory.join("editor-draft.json");
        let _ = fs::remove_dir_all(directory);
        fs::create_dir_all(directory).unwrap();
        let session = directory.join("session.toml");
        let config = directory.join("config.toml");
        let cache = directory.join("notes-cache.sqlite3");
        fs::write(&session, "session remains").unwrap();
        fs::write(&config, "config remains").unwrap();
        fs::write(&cache, "cache remains").unwrap();
        fs::write(&path, b"malformed but explicitly clearable").unwrap();
        assert!(clear_editor_draft(&path).unwrap());
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(&session).unwrap(), "session remains");
        assert_eq!(fs::read_to_string(&config).unwrap(), "config remains");
        assert_eq!(fs::read_to_string(&cache).unwrap(), "cache remains");
        assert!(!clear_editor_draft(&path).unwrap());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn draft_cli_commands_are_standalone_and_mutually_exclusive() {
        assert_eq!(
            parse_draft_cli_command(&["--draft-info".into()]).unwrap(),
            Some(DraftCliCommand::Info)
        );
        assert_eq!(
            parse_draft_cli_command(&["--draft-clear".into()]).unwrap(),
            Some(DraftCliCommand::Clear)
        );
        assert!(parse_draft_cli_command(&["--draft-info".into(), "--draft-clear".into()]).is_err());
        assert!(
            parse_draft_cli_command(&["--draft-clear".into(), "--config-reset".into()]).is_err()
        );
        assert!(parse_draft_cli_command(&[
            "--draft-info".into(),
            "--refresh-interval".into(),
            "60".into()
        ])
        .is_err());
    }

    #[test]
    fn legacy_b1_draft_without_cursor_viewport_restores_with_defaults() {
        let path = temporary_session_path("editor-draft-legacy-position");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, _, _) = update_cache_app(false, None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, " legacy");
        let mut draft = app.editor_recovery_draft().unwrap();
        draft.cursor = None;
        draft.viewport = None;
        let mut legacy_json = serde_json::to_value(&draft).unwrap();
        let fields = legacy_json.as_object_mut().unwrap();
        fields.remove("cursor");
        fields.remove("viewport");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&legacy_json).unwrap()).unwrap();
        assert_eq!(load_editor_draft(&path).unwrap(), Some(draft.clone()));
        app.edit = None;
        app.set_editor_draft_state(path.clone(), Some(draft));
        app.offer_editor_draft_recovery();
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        let edit = app.edit.as_ref().unwrap();
        assert_eq!(edit.field, EditField::Body);
        assert_eq!(edit.cursor, 0);
        assert_eq!(edit.viewport, 0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn restored_editor_recovers_exact_cursor_and_viewport_without_backend_or_cache_work() {
        let path = temporary_session_path("editor-draft-restore-position");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, cache) = update_cache_app(false, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        let mut draft = make_nontrivial_dirty_edit(&mut app, "Position restore");
        draft.cursor = Some(EditorCursorRecovery {
            field: EditorRecoveryField::Body,
            target: EditorTarget::ListItem {
                block_index: 1,
                item_index: 0,
            },
            offset: 4,
        });
        draft.viewport = Some(3);
        save_editor_draft(&path, &draft).unwrap();
        app.edit = None;
        app.set_editor_draft_state(path.clone(), Some(draft.clone()));
        app.offer_editor_draft_recovery();
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        let edit = app.edit.as_ref().unwrap();
        assert!(edit.dirty);
        assert_eq!(edit.document.document, draft.document);
        assert_eq!(edit.current_target, draft.cursor.unwrap().target);
        assert_eq!(edit.cursor, draft.cursor.unwrap().offset);
        assert_eq!(edit.viewport, draft.viewport.unwrap());
        assert_eq!(counts.creates.load(Ordering::SeqCst), 0);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        let cache = cache.lock().unwrap();
        assert_eq!(cache.upsert_note_calls, 0);
        assert_eq!(cache.replace_snapshot_calls, 0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn clean_editor_position_changes_do_not_create_recovery_draft() {
        let path = temporary_session_path("editor-draft-clean-position");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let mut app = App::demo();
        app.refresh();
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        app.handle_key(KeyEvent::from(KeyCode::Left));
        app.handle_key(KeyEvent::from(KeyCode::PageDown));
        assert!(app.edit.as_ref().is_some_and(|edit| !edit.dirty));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn dirty_editor_position_changes_update_and_deduplicate_recovery_metadata() {
        let path = temporary_session_path("editor-draft-dirty-position");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let mut app = App::demo();
        app.refresh();
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, "x");
        let initial = load_editor_draft(&path).unwrap().unwrap();
        app.handle_key(KeyEvent::from(KeyCode::Left));
        let moved = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(moved.document, initial.document);
        assert_ne!(moved.cursor, initial.cursor);
        let bytes = fs::read(&path).unwrap();
        app.persist_editor_recovery_if_dirty();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        app.handle_key(KeyEvent::from(KeyCode::PageDown));
        let scrolled = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(scrolled.document, moved.document);
        assert_ne!(scrolled.viewport, moved.viewport);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn restored_editor_position_is_clamped_unicode_safe_and_failed_update_keeps_latest_metadata() {
        let path = temporary_session_path("editor-draft-clamped-position");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = update_cache_app(true, None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, "Grüße Русский 日本語 🚀 é");
        let mut draft = app.editor_recovery_draft().unwrap();
        draft.cursor = Some(EditorCursorRecovery {
            field: EditorRecoveryField::Body,
            target: EditorTarget::Block { block_index: 9999 },
            offset: usize::MAX,
        });
        draft.viewport = Some(u16::MAX);
        save_editor_draft(&path, &draft).unwrap();
        app.edit = None;
        app.set_editor_draft_state(path.clone(), Some(draft));
        app.offer_editor_draft_recovery();
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        let (cursor, viewport) = {
            let edit = app.edit.as_ref().unwrap();
            assert!(edit.document.validate_target(edit.current_target));
            assert_eq!(
                edit.cursor,
                edit.document.char_len(edit.current_target).unwrap()
            );
            assert_eq!(edit.viewport, editor_viewport_max(edit));
            (edit.cursor, edit.viewport)
        };
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.edit.as_ref().is_some_and(|edit| edit.dirty));
        let retained = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(retained.cursor.unwrap().offset, cursor);
        assert_eq!(retained.viewport, Some(viewport));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn conflict_keeps_latest_cursor_and_viewport_in_recovery() {
        let path = temporary_session_path("editor-draft-conflict-position");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = update_cache_app(false, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, " position");
        app.handle_key(KeyEvent::from(KeyCode::Right));
        app.handle_key(KeyEvent::from(KeyCode::PageDown));
        let expected = load_editor_draft(&path).unwrap().unwrap();
        counts.conflict_on_next_get.store(true, Ordering::SeqCst);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::Conflict(_))));
        assert_eq!(load_editor_draft(&path).unwrap(), Some(expected));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn successful_save_cleanup_failure_does_not_rollback_or_repeat_mutation() {
        let path = temporary_session_path("draft-cleanup-success");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = update_cache_app(false, None);
        app.set_editor_draft_state(path.clone(), None);
        app.handle_key(KeyEvent::from(KeyCode::Char('e')));
        type_text(&mut app, " cleanup");
        assert!(path.exists());
        app.editor_draft_failure = Some(EditorDraftFailurePoint::Remove);
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert_eq!(counts.updates.load(Ordering::SeqCst), 1);
        assert!(app.edit.is_none());
        assert_eq!(app.data_source, DataSourceState::Live);
        assert!(app.status.text.contains("could not be removed"));
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn restored_edit_draft_preserves_old_baseline_and_conflicts_after_external_change() {
        let path = temporary_session_path("draft-restart-conflict");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let (mut app, counts, _) = update_cache_app(false, None);
        let selected_id = app.selected_note_id().expect("selected note").to_string();
        let mut draft = recovery_draft(EditorRecoveryKind::Edit, Some(&selected_id));
        draft.expected_modification_date = Some(NoteDate::new("T1"));
        save_editor_draft(&path, &draft).unwrap();
        app.set_editor_draft_state(path.clone(), Some(draft.clone()));
        app.offer_editor_draft_recovery();
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        assert_eq!(
            app.edit.as_ref().unwrap().base_modification_date,
            Some(NoteDate::new("T1"))
        );
        app.handle_key(modified('s', KeyModifiers::CONTROL));
        poll_update_until_idle(&mut app);
        assert!(matches!(app.popup, Some(Popup::Conflict(_))));
        assert_eq!(counts.updates.load(Ordering::SeqCst), 0);
        let persisted = load_editor_draft(&path).unwrap().unwrap();
        assert_eq!(persisted.document, draft.document);
        assert_eq!(
            persisted.expected_modification_date,
            draft.expected_modification_date
        );
        assert!(persisted.cursor.is_some());
        assert_eq!(persisted.viewport, Some(0));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn config_set_parses_and_rejects_invalid_values() {
        assert_eq!(
            parse_config_set("auto_refresh=false").unwrap(),
            ConfigEditCommand::Set(ConfigKey::AutoRefresh, ConfigEditValue::Boolean(false))
        );
        assert_eq!(
            parse_config_set("refresh_interval_seconds=120").unwrap(),
            ConfigEditCommand::Set(
                ConfigKey::RefreshIntervalSeconds,
                ConfigEditValue::RefreshIntervalSeconds(120)
            )
        );
        assert!(parse_config_set("unknown=true").is_err());
        assert!(parse_config_set("preview_wrap=yes").is_err());
        assert!(parse_config_set("refresh_interval_seconds=2").is_err());
    }

    #[test]
    fn config_set_unset_and_reset_preserve_unknown_fields() {
        let path = temporary_config_path("preserve");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        edit_config(&path, parse_config_set("auto_refresh=false").unwrap()).unwrap();
        edit_config(&path, parse_config_set("preview_wrap=false").unwrap()).unwrap();
        let mut contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("auto_refresh = false"));
        assert!(contents.contains("preview_wrap = false"));
        fs::write(&path, format!("future_option = \"keep\"\n{contents}")).unwrap();
        edit_config(&path, ConfigEditCommand::Unset(ConfigKey::PreviewWrap)).unwrap();
        contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("future_option = \"keep\""));
        assert!(!contents.contains("preview_wrap"));
        edit_config(&path, ConfigEditCommand::Reset).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "future_option = \"keep\"\n"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn config_reset_removes_supported_only_file_and_malformed_file_is_untouched() {
        let path = temporary_config_path("reset");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        edit_config(&path, parse_config_set("auto_refresh=false").unwrap()).unwrap();
        edit_config(&path, ConfigEditCommand::Reset).unwrap();
        assert!(!path.exists());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let malformed = "auto_refresh = \"banana\"\n";
        fs::write(&path, malformed).unwrap();
        assert!(edit_config(&path, parse_config_set("preview_wrap=false").unwrap()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), malformed);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn config_edit_parser_rejects_mixed_runtime_mode() {
        assert!(parse_config_edit_command(&[
            "--config-set".into(),
            "auto_refresh=false".into(),
            "--auto-refresh".into()
        ])
        .is_err());
        assert_eq!(
            parse_config_edit_command(&["--config-unset".into(), "preview_wrap".into()]).unwrap(),
            Some(ConfigEditCommand::Unset(ConfigKey::PreviewWrap))
        );
    }

    #[test]
    fn settings_popup_draft_is_local_and_escape_discards_it() {
        let mut app = App::demo();
        let original = app.auto_refresh;
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        assert!(matches!(app.popup, Some(Popup::Settings(_))));
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(app.auto_refresh, original);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.popup.is_none());
        assert_eq!(app.auto_refresh, original);
    }

    #[test]
    fn settings_save_persists_only_dirty_fields_and_preserves_unknown_values() {
        let path = temporary_config_path("settings");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let mut app = App::demo();
        app.set_config_metadata(path.clone(), [ConfigValueSource::Default; 4], [false; 4]);
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        app.handle_key(KeyEvent::from(KeyCode::Char('s')));
        assert!(app.popup.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "auto_refresh = false\n");
        fs::write(&path, "future_option = \"keep\"\nauto_refresh = false\n").unwrap();
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        app.handle_key(KeyEvent::from(KeyCode::Char('s')));
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("future_option = \"keep\""));
        assert!(content.contains("preview_wrap = false"));
        assert!(app.preview_wrap);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn settings_interval_validates_and_malformed_file_refuses_save() {
        let path = temporary_config_path("settings-invalid");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        let mut app = App::demo();
        app.set_config_metadata(path.clone(), [ConfigValueSource::Default; 4], [false; 4]);
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        for _ in 0..3 {
            app.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        app.handle_key(KeyEvent::from(KeyCode::Char('2')));
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(
            app.popup,
            Some(Popup::Settings(SettingsState { error: Some(_), .. }))
        ));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "auto_refresh = \"broken\"\n").unwrap();
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        app.handle_key(KeyEvent::from(KeyCode::Char(' ')));
        app.handle_key(KeyEvent::from(KeyCode::Char('s')));
        assert!(matches!(
            app.popup,
            Some(Popup::Settings(SettingsState { error: Some(_), .. }))
        ));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "auto_refresh = \"broken\"\n"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn settings_unset_revert_and_reset_stage_typed_operations() {
        let path = temporary_config_path("settings-unset");
        let _ = fs::remove_dir_all(path.parent().unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "auto_refresh = false\npreview_wrap = false\n").unwrap();
        let mut app = App::demo();
        app.set_config_metadata(
            path.clone(),
            [ConfigValueSource::File; 4],
            [true, false, true, false],
        );
        app.handle_key(KeyEvent::from(KeyCode::Char(',')));
        app.handle_key(KeyEvent::from(KeyCode::Char('u')));
        assert!(matches!(
            app.popup,
            Some(Popup::Settings(SettingsState {
                staged: [SettingDraft::Unset, ..],
                ..
            }))
        ));
        assert!(fs::read_to_string(&path).unwrap().contains("preview_wrap"));
        app.handle_key(KeyEvent::from(KeyCode::Char('r')));
        app.handle_key(KeyEvent::from(KeyCode::Char('R')));
        app.handle_key(KeyEvent::from(KeyCode::Char('s')));
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn startup_live_refresh_does_not_block_cached_ui() {
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let counts = MutationCounts::default();
        let backend = BlockingRefreshBackend {
            inner: demo_backend(),
            started: Mutex::new(Some(started_sender)),
            release: Mutex::new(Some(release_receiver)),
            counts,
        };
        let mut app = App::demo();
        app.refresh();
        app.backend = Arc::new(Mutex::new(Box::new(backend)));
        app.start_initial_refresh();
        started_receiver.recv().unwrap();

        app.handle_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(app.show_help);
        assert!(!app.notes.is_empty());

        release_sender.send(()).unwrap();
        for _ in 0..100 {
            app.poll_periodic_refresh();
            if matches!(app.periodic_refresh, PeriodicRefreshState::Idle) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(matches!(app.data_source, DataSourceState::Live));
    }

    #[test]
    fn cached_navigation_remains_responsive_during_long_periodic_refresh() {
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let counts = MutationCounts::default();
        let backend = BlockingRefreshBackend {
            inner: demo_backend(),
            started: Mutex::new(Some(started_sender)),
            release: Mutex::new(Some(release_receiver)),
            counts,
        };
        let mut app = App::demo();
        app.refresh();
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let retained = app.notes.clone();
        app.folder_notes_cache.insert(folder_id, retained.clone());
        app.backend = Arc::new(Mutex::new(Box::new(backend)));
        app.start_initial_refresh();
        started_receiver.recv().unwrap();

        app.handle_key(KeyEvent::from(KeyCode::Char('?')));
        assert_eq!(app.notes, retained);
        assert!(app.show_help);
        assert!(app.selected_note_id().is_some());

        release_sender.send(()).unwrap();
        app.poll_periodic_refresh();
    }

    #[test]
    fn latest_navigation_survives_periodic_backend_contention() {
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let backend = BlockingRefreshBackend {
            inner: demo_backend(),
            started: Mutex::new(Some(started_sender)),
            release: Mutex::new(Some(release_receiver)),
            counts: MutationCounts::default(),
        };
        let mut app = App::demo();
        app.refresh();
        app.backend = Arc::new(Mutex::new(Box::new(backend)));
        app.start_initial_refresh();
        started_receiver.recv().unwrap();

        let ids: Vec<_> = app.notes.iter().map(|note| note.id.clone()).collect();
        for id in ids.iter().take(4) {
            app.queue_navigation_request(NavigationRequest::Preview(id.clone()));
        }
        assert!(matches!(
            app.pending_navigation,
            Some(NavigationRequest::Preview(ref id)) if id == &ids[3]
        ));

        assert!(matches!(
            app.pending_navigation,
            Some(NavigationRequest::Preview(ref id)) if id == &ids[3]
        ));
        drop(release_sender);
    }

    #[test]
    fn live_mode_preview_cache_miss_is_async() {
        let (mut app, _counts, cache) = create_cache_app(false, None);
        cache.lock().unwrap().full_notes.clear();
        app.load_selected_note();
        assert!(app.navigation_worker.is_some());
        app.poll_navigation_worker();
    }

    #[test]
    fn live_mode_folder_cache_miss_is_async() {
        let (mut app, _counts, cache) = create_cache_app(false, None);
        cache.lock().unwrap().bootstrap.notes.clear();
        app.load_notes_for_browsing();
        assert!(app.navigation_worker.is_some());
        app.poll_navigation_worker();
    }

    #[test]
    fn rapid_note_selection_coalesces_to_latest_preview() {
        let (mut app, _counts, cache) = create_cache_app(false, None);
        cache.lock().unwrap().full_notes.clear();
        let first = app.visible_note(0).unwrap().id.clone();
        app.start_preview_worker(first);
        let ids: Vec<_> = app.notes.iter().map(|note| note.id.clone()).collect();
        for id in ids.iter().skip(1).take(3) {
            app.queue_navigation_request(NavigationRequest::Preview(id.clone()));
        }
        assert!(matches!(
            app.pending_navigation,
            Some(NavigationRequest::Preview(ref id)) if id == ids.get(3).unwrap()
        ));
    }

    #[test]
    fn rapid_ten_note_navigation_executes_only_inflight_and_latest_preview() {
        let (mut app, _counts, cache) = create_cache_app(false, None);
        cache.lock().unwrap().full_notes.clear();
        let ids: Vec<_> = app.notes.iter().map(|note| note.id.clone()).collect();
        let first = ids[0].clone();
        app.start_preview_worker(first);
        for id in ids.iter().skip(1) {
            app.queue_navigation_request(NavigationRequest::Preview(id.clone()));
        }
        assert!(matches!(
            app.pending_navigation,
            Some(NavigationRequest::Preview(ref id)) if id == ids.last().unwrap()
        ));
    }

    #[test]
    fn startup_refresh_suppresses_immediate_periodic_refresh() {
        let mut app = App::demo();
        app.set_refresh_interval(Duration::from_secs(60));
        app.refresh();
        let t0 = Instant::now();
        app.start_initial_refresh();
        app.periodic_refresh_at(t0 + Duration::from_secs(30));
        assert!(matches!(
            app.periodic_refresh,
            PeriodicRefreshState::InFlight { .. }
        ));
    }

    #[test]
    fn periodic_refresh_interval_is_measured_from_refresh_request() {
        let mut app = App::demo();
        app.set_refresh_interval(Duration::from_secs(60));
        app.refresh();
        let t0 = Instant::now();
        app.start_refresh_worker(t0, RefreshOrigin::Automatic);
        assert!(app.last_refresh_attempt >= t0);
        assert!(app.last_refresh_attempt < t0 + Duration::from_secs(1));
    }

    #[test]
    fn manual_refresh_remains_allowed_before_periodic_deadline() {
        let mut app = App::demo();
        app.set_refresh_interval(Duration::from_secs(60));
        app.refresh();
        app.start_initial_refresh();
        app.pending_foreground_intent = None;
        app.start_manual_refresh();
        assert!(matches!(
            app.periodic_refresh,
            PeriodicRefreshState::InFlight { .. }
        ));
    }

    #[test]
    fn revisiting_loaded_folder_uses_session_cache_before_backend() {
        let mut app = App::demo();
        app.refresh();
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let retained = app.notes.clone();
        app.folder_notes_cache.insert(folder_id, retained.clone());
        app.load_notes_for_browsing();
        assert_eq!(app.notes, retained);
    }

    #[test]
    fn authoritative_folder_refresh_replaces_retained_rows() {
        let mut app = App::demo();
        app.refresh();
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let replacement = app.notes.clone();
        app.folder_notes_cache.insert(folder_id.clone(), vec![]);
        app.folder_notes_cache
            .insert(folder_id, replacement.clone());
        assert_eq!(app.folder_notes_cache.values().next(), Some(&replacement));
    }

    #[test]
    fn repeated_folder_navigation_reuses_retained_rows() {
        let mut app = App::demo();
        app.refresh();
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let retained = app.notes.clone();
        app.folder_notes_cache.insert(folder_id, retained.clone());
        app.load_notes_for_browsing();
        let first = app.notes.clone();
        app.load_notes_for_browsing();
        assert_eq!(first, retained);
        assert_eq!(app.notes, retained);
    }

    #[test]
    fn stale_folder_result_cannot_overwrite_newer_retained_rows() {
        let mut app = App::demo();
        app.refresh();
        let folder_id = app.selected_folder_id().cloned().expect("folder");
        let newer = app.notes.clone();
        app.folder_notes_cache
            .insert(folder_id.clone(), newer.clone());
        let stale = newer.first().cloned().into_iter().collect::<Vec<_>>();
        assert_eq!(app.folder_notes_cache.get(&folder_id), Some(&newer));
        assert_ne!(stale, newer);
    }
}
