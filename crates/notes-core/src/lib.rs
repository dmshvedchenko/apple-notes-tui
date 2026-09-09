//! Platform-neutral domain types for a future Apple Notes TUI.
//!
//! This crate deliberately has no dependency on AppleScript, `osascript`, or
//! macOS. Backends translate their transport details into these types.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod perf;
pub mod rich_text;
pub use rich_text::{
    parse_notes_html, serialize_notes_html, Block, Inline, ListItem, RichDocument, RichFeature,
    RichTextCapabilities, RichTextError,
};

macro_rules! notes_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

notes_id!(AccountId);
notes_id!(FolderId);
notes_id!(NoteId);
notes_id!(AttachmentId);

/// The original, locale-dependent date string returned by Notes.app.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct NoteDate(String);

impl NoteDate {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NoteDate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub is_default: bool,
    pub is_upgraded: bool,
    pub default_folder_id: Option<FolderId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FolderParent {
    Account { account_id: AccountId },
    Folder { folder_id: FolderId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: AccountId,
    pub name: String,
    pub parent: FolderParent,
    pub shared: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateFolder {
    pub account_id: AccountId,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateChildFolder {
    pub account_id: AccountId,
    pub parent_folder_id: FolderId,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReparentFolder {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub new_parent_folder_id: Option<FolderId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameFolder {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteFolder {
    pub account_id: AccountId,
    pub folder_id: FolderId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletedFolder {
    pub account_id: AccountId,
    pub folder_id: FolderId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NoteSummary {
    pub id: NoteId,
    pub folder_id: FolderId,
    pub name: String,
    pub creation_date: NoteDate,
    pub modification_date: NoteDate,
    pub password_protected: bool,
    pub shared: bool,
    pub attachment_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttachmentSummary {
    pub id: AttachmentId,
    pub note_id: NoteId,
    pub display_name: String,
    pub kind: AttachmentKind,
    pub content_identifier: Option<String>,
    pub source_url: Option<String>,
    pub creation_date: NoteDate,
    pub modification_date: NoteDate,
    pub shared: bool,
    pub preview_status: AttachmentAccessStatus,
    pub export_status: AttachmentAccessStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttachmentMetadata {
    pub id: AttachmentId,
    pub note_id: NoteId,
    pub display_name: String,
    pub content_identifier: Option<String>,
    pub source_url: Option<String>,
    pub creation_date: NoteDate,
    pub modification_date: NoteDate,
    pub shared: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AttachmentKind {
    Image,
    Pdf,
    Document,
    Audio,
    Video,
    Archive,
    Url,
    Unknown,
}

impl fmt::Display for AttachmentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Image => "image",
            Self::Pdf => "PDF",
            Self::Document => "document",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Archive => "archive",
            Self::Url => "URL",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AttachmentUnavailableReason {
    BackendUnsupported,
    ProtectedNote,
    UnsupportedKind,
    UnsafeUrl,
}

impl fmt::Display for AttachmentUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BackendUnsupported => "backend does not support this operation",
            Self::ProtectedNote => "password-protected note",
            Self::UnsupportedKind => "unsupported attachment kind",
            Self::UnsafeUrl => "URL attachments are metadata-only",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AttachmentAccessStatus {
    Available,
    Unavailable(AttachmentUnavailableReason),
}

impl AttachmentAccessStatus {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

impl fmt::Display for AttachmentAccessStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available => formatter.write_str("available"),
            Self::Unavailable(reason) => write!(formatter, "unavailable: {reason}"),
        }
    }
}

impl AttachmentSummary {
    pub fn from_apple_events_metadata(
        metadata: AttachmentMetadata,
        capabilities: AttachmentCapabilities,
    ) -> Self {
        let kind = classify_attachment_kind(&metadata.display_name, metadata.source_url.as_deref());
        let preview_status = attachment_preview_status(kind, capabilities);
        let export_status = attachment_export_status(kind, capabilities);
        Self {
            id: metadata.id,
            note_id: metadata.note_id,
            display_name: metadata.display_name,
            kind,
            content_identifier: metadata.content_identifier,
            source_url: metadata.source_url,
            creation_date: metadata.creation_date,
            modification_date: metadata.modification_date,
            shared: metadata.shared,
            preview_status,
            export_status,
        }
    }
}

pub fn classify_attachment_kind(name: &str, source_url: Option<&str>) -> AttachmentKind {
    if source_url.is_some() {
        return AttachmentKind::Url;
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "bmp" | "gif" | "heic" | "heif" | "jpeg" | "jpg" | "png" | "tif" | "tiff" | "webp" => {
            AttachmentKind::Image
        }
        "pdf" => AttachmentKind::Pdf,
        "csv" | "doc" | "docx" | "key" | "md" | "numbers" | "pages" | "ppt" | "pptx" | "rtf"
        | "rtfd" | "txt" | "xls" | "xlsx" => AttachmentKind::Document,
        "aac" | "aiff" | "flac" | "m4a" | "mp3" | "wav" => AttachmentKind::Audio,
        "avi" | "m4v" | "mkv" | "mov" | "mp4" | "webm" => AttachmentKind::Video,
        "7z" | "bz2" | "gz" | "rar" | "tar" | "tgz" | "zip" => AttachmentKind::Archive,
        _ => AttachmentKind::Unknown,
    }
}

fn attachment_preview_status(
    kind: AttachmentKind,
    capabilities: AttachmentCapabilities,
) -> AttachmentAccessStatus {
    if !capabilities.notes_app_preview {
        AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::BackendUnsupported)
    } else {
        match kind {
            AttachmentKind::Url => {
                AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::UnsafeUrl)
            }
            AttachmentKind::Unknown => {
                AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::UnsupportedKind)
            }
            _ => AttachmentAccessStatus::Available,
        }
    }
}

fn attachment_export_status(
    kind: AttachmentKind,
    capabilities: AttachmentCapabilities,
) -> AttachmentAccessStatus {
    if !capabilities.export_copy {
        AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::BackendUnsupported)
    } else if kind == AttachmentKind::Url {
        AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::UnsafeUrl)
    } else {
        AttachmentAccessStatus::Available
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachmentCapabilities {
    pub metadata: bool,
    pub notes_app_preview: bool,
    pub export_copy: bool,
    pub exposes_local_file_path: bool,
}

impl AttachmentCapabilities {
    pub const fn unsupported() -> Self {
        Self {
            metadata: false,
            notes_app_preview: false,
            export_copy: false,
            exposes_local_file_path: false,
        }
    }

    pub const fn notes_apple_events() -> Self {
        Self {
            metadata: true,
            notes_app_preview: true,
            export_copy: true,
            exposes_local_file_path: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendCapabilities {
    pub rich_text: RichTextCapabilities,
    pub attachments: AttachmentCapabilities,
    pub delete: bool,
}

impl BackendCapabilities {
    pub const fn new(rich_text: RichTextCapabilities) -> Self {
        Self {
            rich_text,
            attachments: AttachmentCapabilities::unsupported(),
            delete: true,
        }
    }

    pub const fn with_attachments(mut self, attachments: AttachmentCapabilities) -> Self {
        self.attachments = attachments;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachmentExportResult {
    pub note_id: NoteId,
    pub attachment_id: AttachmentId,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Note {
    pub summary: NoteSummary,
    pub account_id: AccountId,
    pub body_html: String,
    pub plaintext: String,
    pub attachments: Vec<AttachmentSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Editability {
    PlainText,
    RichTextSupported,
    ReadOnlyUnsupported { reasons: Vec<String> },
}

pub fn classify_editability(note: &Note) -> Editability {
    let mut reasons = Vec::new();
    if note.summary.attachment_count > 0 || !note.attachments.is_empty() {
        reasons.push("note contains attachments".into());
    }
    let lower = note.body_html.to_ascii_lowercase();
    for marker in [
        "<img",
        "<table",
        "<object",
        "<embed",
        "attachment",
        "checklist",
        "checkbox",
        "<input",
    ] {
        if lower.contains(marker) {
            reasons.push(format!("unsupported content: {marker}"));
        }
    }
    match parse_notes_html(&note.body_html) {
        Ok(document) => {
            if reasons.is_empty() {
                if document.blocks.iter().all(|block| matches!(block, Block::Paragraph(inlines) if inlines.iter().all(|inline| matches!(inline, Inline::Text(_))))) {
                    Editability::PlainText
                } else { Editability::RichTextSupported }
            } else {
                Editability::ReadOnlyUnsupported { reasons }
            }
        }
        Err(error) => {
            reasons.push(error.to_string());
            Editability::ReadOnlyUnsupported { reasons }
        }
    }
}

pub fn plaintext_to_safe_html(input: &str) -> String {
    input
        .split('\n')
        .map(|line| format!("<div>{}</div>", escape_html(line)))
        .collect::<Vec<_>>()
        .join("\n")
}
fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotesQuery {
    pub account_id: Option<AccountId>,
    pub folder_id: Option<FolderId>,
    /// `None` means use the backend default; `Some(0)` requests all matches.
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotesPage {
    pub items: Vec<NoteSummary>,
    pub total: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateNote {
    pub folder_id: FolderId,
    pub name: String,
    pub body_html: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateNote {
    pub id: NoteId,
    pub name: Option<String>,
    pub body_html: Option<String>,
    /// Reserved for a future backend-side optimistic concurrency check.
    pub expected_modification_date: Option<NoteDate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoveNote {
    pub id: NoteId,
    pub destination_folder_id: FolderId,
    pub expected_modification_date: Option<NoteDate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteNote {
    pub id: NoteId,
    pub expected_modification_date: Option<NoteDate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeleteDisposition {
    /// Notes.app accepted the delete request but owns recovery/permanent-delete behavior.
    DelegatedToNotesApp,
    MovedToRecentlyDeleted {
        folder_id: FolderId,
    },
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteResult {
    pub id: NoteId,
    pub source_folder_id: FolderId,
    pub disposition: DeleteDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityKind {
    Account,
    Folder,
    Note,
    Attachment,
    Unknown,
}

impl fmt::Display for EntityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Account => "account",
            Self::Folder => "folder",
            Self::Note => "note",
            Self::Attachment => "attachment",
            Self::Unknown => "entity",
        })
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NotesError {
    #[error("{kind} not found: {id}")]
    NotFound { kind: EntityKind, id: String },
    #[error("Notes automation permission denied: {0}")]
    PermissionDenied(String),
    #[error("Notes backend timed out during {operation}")]
    Timeout { operation: String },
    #[error("invalid backend response: {0}")]
    InvalidResponse(String),
    #[error("Notes conflict for {id}: {message}")]
    Conflict { id: NoteId, message: String },
    #[error("rich content cannot be saved losslessly: {features}", features = display_features(.features))]
    UnsupportedRichContent { features: Vec<RichFeature> },
    #[error("password-protected note is unavailable: {id}")]
    ProtectedNote { id: NoteId },
    #[error("attachment {id} cannot be {operation}: {reason}")]
    AttachmentUnavailable {
        id: AttachmentId,
        operation: &'static str,
        reason: AttachmentUnavailableReason,
    },
    #[error("operation cancelled")]
    Cancelled,
    #[error("Notes backend error: {0}")]
    Backend(String),
}

fn display_features(features: &[RichFeature]) -> String {
    features
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The only API a future TUI needs to know.
pub trait NotesBackend: Send {
    fn capabilities(&self) -> BackendCapabilities;
    fn accounts(&self) -> Result<Vec<Account>, NotesError>;
    fn folders(&self, account: Option<&AccountId>) -> Result<Vec<Folder>, NotesError>;
    fn notes(&self, query: &NotesQuery) -> Result<NotesPage, NotesError>;
    fn get_note(&self, id: &NoteId) -> Result<Note, NotesError>;
    /// Read-only refresh hooks may opt into cooperative cancellation. Normal
    /// foreground callers continue to use the non-cancellable methods above.
    fn accounts_with_cancel(
        &self,
        _cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Account>, NotesError> {
        self.accounts()
    }
    fn folders_with_cancel(
        &self,
        account: Option<&AccountId>,
        _cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Folder>, NotesError> {
        self.folders(account)
    }
    fn notes_with_cancel(
        &self,
        query: &NotesQuery,
        _cancel: Option<&AtomicBool>,
    ) -> Result<NotesPage, NotesError> {
        self.notes(query)
    }
    fn get_note_with_cancel(
        &self,
        id: &NoteId,
        _cancel: Option<&AtomicBool>,
    ) -> Result<Note, NotesError> {
        self.get_note(id)
    }
    fn preview_attachment(
        &self,
        note_id: &NoteId,
        attachment_id: &AttachmentId,
    ) -> Result<(), NotesError>;
    fn export_attachment(
        &self,
        note_id: &NoteId,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentExportResult, NotesError>;
    fn create_note(&self, request: &CreateNote) -> Result<Note, NotesError>;
    fn create_folder(&self, _request: &CreateFolder) -> Result<Folder, NotesError> {
        Err(NotesError::Backend(
            "folder creation is not supported by this backend".into(),
        ))
    }
    fn create_child_folder(&self, _request: &CreateChildFolder) -> Result<Folder, NotesError> {
        Err(NotesError::Backend(
            "child-folder creation is not supported by this backend".into(),
        ))
    }
    fn reparent_folder(&self, _request: &ReparentFolder) -> Result<Folder, NotesError> {
        Err(NotesError::Backend(
            "folder reparenting is not supported by this backend".into(),
        ))
    }
    fn rename_folder(&self, _request: &RenameFolder) -> Result<Folder, NotesError> {
        Err(NotesError::Backend(
            "folder rename is not supported by this backend".into(),
        ))
    }
    fn delete_folder(&self, _request: &DeleteFolder) -> Result<DeletedFolder, NotesError> {
        Err(NotesError::Backend(
            "folder deletion is not supported by this backend".into(),
        ))
    }
    fn update_note(&self, request: &UpdateNote) -> Result<Note, NotesError>;
    fn move_note(&self, request: &MoveNote) -> Result<Note, NotesError>;
    fn delete_note(&self, request: &DeleteNote) -> Result<DeleteResult, NotesError>;
}

#[derive(Debug, Default)]
pub struct MockNotesBackend {
    state: Mutex<MockState>,
}
#[derive(Debug, Default)]
struct MockState {
    accounts: Vec<Account>,
    folders: Vec<Folder>,
    notes: HashMap<NoteId, Note>,
    revision: usize,
}
impl MockNotesBackend {
    pub fn new(accounts: Vec<Account>, folders: Vec<Folder>, notes: HashMap<NoteId, Note>) -> Self {
        Self {
            state: Mutex::new(MockState {
                accounts,
                folders,
                notes,
                revision: 0,
            }),
        }
    }
}

impl NotesBackend for MockNotesBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new(RichTextCapabilities::all_supported())
            .with_attachments(AttachmentCapabilities::notes_apple_events())
    }

    fn accounts(&self) -> Result<Vec<Account>, NotesError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?
            .accounts
            .clone())
    }

    fn folders(&self, account: Option<&AccountId>) -> Result<Vec<Folder>, NotesError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?
            .folders
            .iter()
            .filter(|folder| account.is_none_or(|id| folder.account_id == *id))
            .cloned()
            .collect())
    }

    fn notes(&self, query: &NotesQuery) -> Result<NotesPage, NotesError> {
        let state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let mut matches: Vec<_> = state
            .notes
            .values()
            .filter(|note| {
                query
                    .folder_id
                    .as_ref()
                    .is_none_or(|id| note.summary.folder_id == *id)
            })
            .filter(|note| {
                query
                    .account_id
                    .as_ref()
                    .is_none_or(|id| note.account_id == *id)
            })
            .map(|note| note.summary.clone())
            .collect();
        // Keep the mock deterministic so UI selection behaviour is testable.
        matches.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        let total = matches.len();
        let items = match query.limit {
            None | Some(0) => matches.clone(),
            Some(limit) => matches.into_iter().take(limit).collect(),
        };
        Ok(NotesPage {
            truncated: items.len() < total,
            items,
            total,
        })
    }

    fn get_note(&self, id: &NoteId) -> Result<Note, NotesError> {
        let note = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?
            .notes
            .get(id)
            .cloned()
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Note,
                id: id.to_string(),
            })?;
        if note.summary.password_protected {
            Err(NotesError::ProtectedNote { id: id.clone() })
        } else {
            Ok(note)
        }
    }

    fn preview_attachment(
        &self,
        note_id: &NoteId,
        attachment_id: &AttachmentId,
    ) -> Result<(), NotesError> {
        let note = self.get_note(note_id)?;
        let attachment = note
            .attachments
            .iter()
            .find(|attachment| attachment.id == *attachment_id)
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Attachment,
                id: attachment_id.to_string(),
            })?;
        match attachment.preview_status {
            AttachmentAccessStatus::Available => Ok(()),
            AttachmentAccessStatus::Unavailable(reason) => Err(NotesError::AttachmentUnavailable {
                id: attachment_id.clone(),
                operation: "previewed",
                reason,
            }),
        }
    }

    fn export_attachment(
        &self,
        note_id: &NoteId,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentExportResult, NotesError> {
        let note = self.get_note(note_id)?;
        let attachment = note
            .attachments
            .iter()
            .find(|attachment| attachment.id == *attachment_id)
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Attachment,
                id: attachment_id.to_string(),
            })?;
        match attachment.export_status {
            AttachmentAccessStatus::Available => Ok(AttachmentExportResult {
                note_id: note_id.clone(),
                attachment_id: attachment_id.clone(),
                destination: PathBuf::from("/mock-export").join(&attachment.display_name),
            }),
            AttachmentAccessStatus::Unavailable(reason) => Err(NotesError::AttachmentUnavailable {
                id: attachment_id.clone(),
                operation: "exported",
                reason,
            }),
        }
    }

    fn create_note(&self, request: &CreateNote) -> Result<Note, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        state.revision += 1;
        let id = NoteId::new(format!("mock-note-{}", state.revision));
        let date = NoteDate::new(format!("mock-revision-{}", state.revision));
        let account_id = state
            .folders
            .iter()
            .find(|folder| folder.id == request.folder_id)
            .map(|folder| folder.account_id.clone())
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Folder,
                id: request.folder_id.to_string(),
            })?;
        let note = Note {
            summary: NoteSummary {
                id: id.clone(),
                folder_id: request.folder_id.clone(),
                name: request.name.clone(),
                creation_date: date.clone(),
                modification_date: date,
                password_protected: false,
                shared: false,
                attachment_count: 0,
            },
            account_id,
            body_html: request.body_html.clone(),
            plaintext: html_to_plaintext(&request.body_html),
            attachments: vec![],
        };
        state.notes.insert(id, note.clone());
        Ok(note)
    }

    fn create_folder(&self, request: &CreateFolder) -> Result<Folder, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        if !state
            .accounts
            .iter()
            .any(|account| account.id == request.account_id)
        {
            return Err(NotesError::NotFound {
                kind: EntityKind::Account,
                id: request.account_id.to_string(),
            });
        }
        state.revision += 1;
        let folder = Folder {
            id: FolderId::new(format!("mock-folder-{}", state.revision)),
            account_id: request.account_id.clone(),
            name: request.name.clone(),
            parent: FolderParent::Account {
                account_id: request.account_id.clone(),
            },
            shared: false,
        };
        state.folders.push(folder.clone());
        Ok(folder)
    }
    fn create_child_folder(&self, request: &CreateChildFolder) -> Result<Folder, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        if !state
            .accounts
            .iter()
            .any(|account| account.id == request.account_id)
        {
            return Err(NotesError::NotFound {
                kind: EntityKind::Account,
                id: request.account_id.to_string(),
            });
        }
        if !state.folders.iter().any(|folder| {
            folder.id == request.parent_folder_id && folder.account_id == request.account_id
        }) {
            return Err(NotesError::NotFound {
                kind: EntityKind::Folder,
                id: request.parent_folder_id.to_string(),
            });
        }
        state.revision += 1;
        let folder = Folder {
            id: FolderId::new(format!("mock-folder-{}", state.revision)),
            account_id: request.account_id.clone(),
            name: request.name.clone(),
            parent: FolderParent::Folder {
                folder_id: request.parent_folder_id.clone(),
            },
            shared: false,
        };
        state.folders.push(folder.clone());
        Ok(folder)
    }
    fn reparent_folder(&self, request: &ReparentFolder) -> Result<Folder, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let source_index = state
            .folders
            .iter()
            .position(|folder| {
                folder.id == request.folder_id && folder.account_id == request.account_id
            })
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Folder,
                id: request.folder_id.to_string(),
            })?;
        if let Some(parent_id) = &request.new_parent_folder_id {
            if parent_id == &request.folder_id {
                return Err(NotesError::Backend(
                    "folder cannot be its own parent".into(),
                ));
            }
            let parent_exists = state
                .folders
                .iter()
                .any(|folder| folder.id == *parent_id && folder.account_id == request.account_id);
            if !parent_exists {
                return Err(NotesError::NotFound {
                    kind: EntityKind::Folder,
                    id: parent_id.to_string(),
                });
            }
            let mut ancestor = Some(parent_id.clone());
            while let Some(current_id) = ancestor {
                if current_id == request.folder_id {
                    return Err(NotesError::Backend(
                        "folder cannot be reparented below its descendant".into(),
                    ));
                }
                ancestor = state
                    .folders
                    .iter()
                    .find(|folder| folder.id == current_id)
                    .and_then(|folder| match &folder.parent {
                        FolderParent::Account { .. } => None,
                        FolderParent::Folder { folder_id } => Some(folder_id.clone()),
                    });
            }
        }
        let folder = &mut state.folders[source_index];
        folder.parent = match &request.new_parent_folder_id {
            Some(folder_id) => FolderParent::Folder {
                folder_id: folder_id.clone(),
            },
            None => FolderParent::Account {
                account_id: request.account_id.clone(),
            },
        };
        Ok(folder.clone())
    }
    fn rename_folder(&self, request: &RenameFolder) -> Result<Folder, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let folder = state
            .folders
            .iter_mut()
            .find(|folder| {
                folder.id == request.folder_id && folder.account_id == request.account_id
            })
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Folder,
                id: request.folder_id.to_string(),
            })?;
        folder.name = request.name.clone();
        Ok(folder.clone())
    }
    fn delete_folder(&self, request: &DeleteFolder) -> Result<DeletedFolder, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let index = state
            .folders
            .iter()
            .position(|folder| {
                folder.id == request.folder_id && folder.account_id == request.account_id
            })
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Folder,
                id: request.folder_id.to_string(),
            })?;
        let has_notes = state
            .notes
            .values()
            .any(|note| note.summary.folder_id == request.folder_id);
        let has_children = state.folders.iter().any(|folder| {
            matches!(&folder.parent, FolderParent::Folder { folder_id } if folder_id == &request.folder_id)
        });
        if has_notes || has_children {
            return Err(NotesError::Backend(
                "folder deletion requires an empty folder with no child folders".into(),
            ));
        }
        state.folders.remove(index);
        Ok(DeletedFolder {
            account_id: request.account_id.clone(),
            folder_id: request.folder_id.clone(),
        })
    }
    fn update_note(&self, request: &UpdateNote) -> Result<Note, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let existing =
            state
                .notes
                .get(&request.id)
                .cloned()
                .ok_or_else(|| NotesError::NotFound {
                    kind: EntityKind::Note,
                    id: request.id.to_string(),
                })?;
        if request
            .expected_modification_date
            .as_ref()
            .is_some_and(|date| *date != existing.summary.modification_date)
        {
            return Err(NotesError::Conflict {
                id: request.id.clone(),
                message: "mock revision changed".into(),
            });
        }
        state.revision += 1;
        let modification_date = NoteDate::new(format!("mock-revision-{}", state.revision));
        let note = state
            .notes
            .get_mut(&request.id)
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Note,
                id: request.id.to_string(),
            })?;
        if let Some(name) = &request.name {
            note.summary.name = name.clone();
        }
        if let Some(body) = &request.body_html {
            note.body_html = body.clone();
            note.plaintext = html_to_plaintext(body);
        }
        note.summary.modification_date = modification_date;
        Ok(note.clone())
    }
    fn move_note(&self, request: &MoveNote) -> Result<Note, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let destination = state
            .folders
            .iter()
            .find(|folder| folder.id == request.destination_folder_id)
            .cloned()
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Folder,
                id: request.destination_folder_id.to_string(),
            })?;
        let note = state
            .notes
            .get_mut(&request.id)
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Note,
                id: request.id.to_string(),
            })?;
        note.summary.folder_id = destination.id;
        note.account_id = destination.account_id;
        Ok(note.clone())
    }
    fn delete_note(&self, request: &DeleteNote) -> Result<DeleteResult, NotesError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| NotesError::Backend("mock lock poisoned".into()))?;
        let request_id = request.id.clone();
        let note = state
            .notes
            .remove(&request_id)
            .ok_or_else(|| NotesError::NotFound {
                kind: EntityKind::Note,
                id: request_id.to_string(),
            })?;
        Ok(DeleteResult {
            id: request_id,
            source_folder_id: note.summary.folder_id,
            disposition: DeleteDisposition::DelegatedToNotesApp,
        })
    }
}
fn html_to_plaintext(input: &str) -> String {
    input
        .replace("<div>", "")
        .replace("</div>", "\n")
        .replace("<br>", "\n")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .trim_end()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_preserve_the_original_value() {
        let id = NoteId::new("x-coredata://account/ICNote/p7");
        assert_eq!(id.to_string(), "x-coredata://account/ICNote/p7");
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"x-coredata://account/ICNote/p7\""
        );
    }

    #[test]
    fn mock_filters_and_reads_notes() {
        let account_id = AccountId::from("account-1");
        let folder_id = FolderId::from("folder-1");
        let id = NoteId::from("note-1");
        let note = Note {
            summary: NoteSummary {
                id: id.clone(),
                folder_id: folder_id.clone(),
                name: "Sample".into(),
                creation_date: NoteDate::new("date"),
                modification_date: NoteDate::new("date"),
                password_protected: false,
                shared: false,
                attachment_count: 0,
            },
            account_id,
            body_html: "<div>Sample</div>".into(),
            plaintext: "Sample".into(),
            attachments: vec![],
        };
        let backend = MockNotesBackend::new(vec![], vec![], HashMap::from([(id.clone(), note)]));
        assert_eq!(
            backend
                .notes(&NotesQuery {
                    folder_id: Some(folder_id),
                    ..Default::default()
                })
                .unwrap()
                .total,
            1
        );
        assert_eq!(backend.get_note(&id).unwrap().plaintext, "Sample");
        assert_eq!(
            backend
                .notes(&NotesQuery {
                    limit: Some(0),
                    ..Default::default()
                })
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[test]
    fn mock_backend_advertises_the_complete_editor_feature_set() {
        let capabilities = MockNotesBackend::default().capabilities();
        let rich = capabilities.rich_text;
        for feature in [
            RichFeature::Heading1,
            RichFeature::Heading2,
            RichFeature::Heading3,
            RichFeature::Bold,
            RichFeature::Italic,
            RichFeature::Underline,
            RichFeature::Hyperlink,
            RichFeature::Quote,
            RichFeature::Code,
            RichFeature::BulletList,
            RichFeature::NumberedList,
            RichFeature::MixedAdjacentListTypes,
        ] {
            assert!(rich.supports(feature), "{feature}");
        }
        assert!(capabilities.attachments.metadata);
        assert!(capabilities.attachments.notes_app_preview);
        assert!(capabilities.attachments.export_copy);
        assert!(!capabilities.attachments.exposes_local_file_path);
        assert!(capabilities.delete);
    }

    #[test]
    fn mock_create_child_folder_sets_exact_parent_stable_id() {
        let (account_a, account_b, parent, foreign_parent) = folder_delete_fixture();
        let backend = MockNotesBackend::new(
            vec![account_a.clone(), account_b],
            vec![parent.clone(), foreign_parent],
            HashMap::new(),
        );
        let child = backend
            .create_child_folder(&CreateChildFolder {
                account_id: account_a.id.clone(),
                parent_folder_id: parent.id.clone(),
                name: "Подпапка ; \"quoted\"".into(),
            })
            .unwrap();
        assert_ne!(child.id, parent.id);
        assert_eq!(child.account_id, account_a.id);
        assert_eq!(
            child.parent,
            FolderParent::Folder {
                folder_id: parent.id
            }
        );
    }

    #[test]
    fn mock_create_child_folder_rejects_cross_account_parent() {
        let (account_a, account_b, _parent, foreign_parent) = folder_delete_fixture();
        let backend = MockNotesBackend::new(
            vec![account_a.clone(), account_b],
            vec![foreign_parent.clone()],
            HashMap::new(),
        );
        assert!(matches!(
            backend.create_child_folder(&CreateChildFolder {
                account_id: account_a.id,
                parent_folder_id: foreign_parent.id,
                name: "Nope".into(),
            }),
            Err(NotesError::NotFound {
                kind: EntityKind::Folder,
                ..
            })
        ));
    }

    #[test]
    fn mock_reparent_folder_preserves_source_id_and_rejects_cycles() {
        let (account, _other, root, _foreign) = folder_delete_fixture();
        let child = Folder {
            id: FolderId::from("child"),
            account_id: account.id.clone(),
            name: "Child".into(),
            parent: FolderParent::Folder {
                folder_id: root.id.clone(),
            },
            shared: false,
        };
        let backend = MockNotesBackend::new(
            vec![account.clone()],
            vec![root.clone(), child.clone()],
            HashMap::new(),
        );
        assert!(matches!(
            backend.reparent_folder(&ReparentFolder {
                account_id: account.id.clone(),
                folder_id: root.id.clone(),
                new_parent_folder_id: Some(child.id.clone()),
            }),
            Err(NotesError::Backend(message)) if message.contains("descendant")
        ));
        let reparented = backend
            .reparent_folder(&ReparentFolder {
                account_id: account.id.clone(),
                folder_id: child.id.clone(),
                new_parent_folder_id: None,
            })
            .unwrap();
        assert_eq!(reparented.id, child.id);
        assert_eq!(
            reparented.parent,
            FolderParent::Account {
                account_id: account.id
            }
        );
    }

    #[test]
    fn mock_rename_folder_rejects_cross_account_target() {
        let account_a = Account {
            id: AccountId::from("account-a"),
            name: "A".into(),
            is_default: false,
            is_upgraded: false,
            default_folder_id: None,
        };
        let account_b = Account {
            id: AccountId::from("account-b"),
            name: "B".into(),
            is_default: false,
            is_upgraded: false,
            default_folder_id: None,
        };
        let folder_b = Folder {
            id: FolderId::from("folder-b"),
            account_id: account_b.id.clone(),
            name: "B folder".into(),
            parent: FolderParent::Account {
                account_id: account_b.id.clone(),
            },
            shared: false,
        };
        let backend = MockNotesBackend::new(
            vec![account_a.clone(), account_b],
            vec![folder_b.clone()],
            HashMap::new(),
        );
        let error = backend
            .rename_folder(&RenameFolder {
                account_id: account_a.id,
                folder_id: folder_b.id.clone(),
                name: "Wrong account rename".into(),
            })
            .expect_err("cross-account target must be rejected");
        assert!(matches!(
            error,
            NotesError::NotFound {
                kind: EntityKind::Folder,
                ..
            }
        ));
        assert_eq!(
            backend.folders(Some(&folder_b.account_id)).unwrap()[0].name,
            "B folder"
        );
    }

    fn folder_delete_fixture() -> (Account, Account, Folder, Folder) {
        let account_a = Account {
            id: AccountId::from("account-a"),
            name: "A".into(),
            is_default: false,
            is_upgraded: false,
            default_folder_id: None,
        };
        let account_b = Account {
            id: AccountId::from("account-b"),
            name: "B".into(),
            is_default: false,
            is_upgraded: false,
            default_folder_id: None,
        };
        let folder_a = Folder {
            id: FolderId::from("folder-a"),
            account_id: account_a.id.clone(),
            name: "A folder".into(),
            parent: FolderParent::Account {
                account_id: account_a.id.clone(),
            },
            shared: false,
        };
        let folder_b = Folder {
            id: FolderId::from("folder-b"),
            account_id: account_b.id.clone(),
            name: "B folder".into(),
            parent: FolderParent::Account {
                account_id: account_b.id.clone(),
            },
            shared: false,
        };
        (account_a, account_b, folder_a, folder_b)
    }

    #[test]
    fn mock_delete_folder_removes_exact_stable_id_target() {
        let (account_a, account_b, folder_a, folder_b) = folder_delete_fixture();
        let backend = MockNotesBackend::new(
            vec![account_a.clone(), account_b.clone()],
            vec![folder_a.clone(), folder_b.clone()],
            HashMap::new(),
        );
        assert_eq!(
            backend
                .delete_folder(&DeleteFolder {
                    account_id: account_a.id.clone(),
                    folder_id: folder_a.id.clone()
                })
                .unwrap(),
            DeletedFolder {
                account_id: account_a.id,
                folder_id: folder_a.id
            }
        );
        assert_eq!(
            backend.folders(Some(&account_b.id)).unwrap(),
            vec![folder_b]
        );
    }

    #[test]
    fn mock_delete_folder_rejects_cross_account_target() {
        let (account_a, account_b, _folder_a, folder_b) = folder_delete_fixture();
        let backend = MockNotesBackend::new(
            vec![account_a.clone(), account_b],
            vec![folder_b.clone()],
            HashMap::new(),
        );
        assert!(matches!(
            backend.delete_folder(&DeleteFolder {
                account_id: account_a.id,
                folder_id: folder_b.id.clone()
            }),
            Err(NotesError::NotFound {
                kind: EntityKind::Folder,
                ..
            })
        ));
        assert_eq!(
            backend.folders(Some(&folder_b.account_id)).unwrap(),
            vec![folder_b]
        );
    }

    #[test]
    fn mock_delete_folder_missing_or_non_empty_target_fails() {
        let (account_a, _account_b, folder_a, _folder_b) = folder_delete_fixture();
        let backend = MockNotesBackend::new(
            vec![account_a.clone()],
            vec![folder_a.clone()],
            HashMap::new(),
        );
        assert!(matches!(
            backend.delete_folder(&DeleteFolder {
                account_id: account_a.id.clone(),
                folder_id: FolderId::from("missing")
            }),
            Err(NotesError::NotFound {
                kind: EntityKind::Folder,
                ..
            })
        ));
        let note = Note {
            summary: NoteSummary {
                id: NoteId::from("note-a"),
                folder_id: folder_a.id.clone(),
                name: "note".into(),
                creation_date: NoteDate::new("date"),
                modification_date: NoteDate::new("date"),
                password_protected: false,
                shared: false,
                attachment_count: 0,
            },
            account_id: account_a.id.clone(),
            body_html: String::new(),
            plaintext: String::new(),
            attachments: vec![],
        };
        let non_empty = MockNotesBackend::new(
            vec![account_a.clone()],
            vec![folder_a.clone()],
            HashMap::from([(note.summary.id.clone(), note)]),
        );
        assert!(
            matches!(non_empty.delete_folder(&DeleteFolder { account_id: account_a.id.clone(), folder_id: folder_a.id.clone() }), Err(NotesError::Backend(message)) if message.contains("empty folder"))
        );
        let child = Folder {
            id: FolderId::from("child"),
            account_id: account_a.id.clone(),
            name: "child".into(),
            parent: FolderParent::Folder {
                folder_id: folder_a.id.clone(),
            },
            shared: false,
        };
        let with_child = MockNotesBackend::new(
            vec![account_a.clone()],
            vec![folder_a.clone(), child],
            HashMap::new(),
        );
        assert!(
            matches!(with_child.delete_folder(&DeleteFolder { account_id: account_a.id, folder_id: folder_a.id }), Err(NotesError::Backend(message)) if message.contains("empty folder"))
        );
    }

    #[test]
    fn attachment_kind_and_access_are_derived_only_from_exposed_metadata() {
        let date = NoteDate::new("date");
        let pdf = AttachmentSummary::from_apple_events_metadata(
            AttachmentMetadata {
                id: AttachmentId::from("pdf"),
                note_id: NoteId::from("note"),
                display_name: "Manual.PDF".into(),
                content_identifier: Some("cid:pdf".into()),
                source_url: None,
                creation_date: date.clone(),
                modification_date: date.clone(),
                shared: false,
            },
            AttachmentCapabilities::notes_apple_events(),
        );
        assert_eq!(pdf.kind, AttachmentKind::Pdf);
        assert_eq!(pdf.preview_status, AttachmentAccessStatus::Available);
        assert_eq!(pdf.export_status, AttachmentAccessStatus::Available);

        let unknown = AttachmentSummary::from_apple_events_metadata(
            AttachmentMetadata {
                id: AttachmentId::from("unknown"),
                note_id: NoteId::from("note"),
                display_name: "Opaque payload".into(),
                content_identifier: None,
                source_url: None,
                creation_date: date.clone(),
                modification_date: date.clone(),
                shared: false,
            },
            AttachmentCapabilities::notes_apple_events(),
        );
        assert_eq!(unknown.kind, AttachmentKind::Unknown);
        assert_eq!(
            unknown.preview_status,
            AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::UnsupportedKind)
        );
        assert_eq!(unknown.export_status, AttachmentAccessStatus::Available);

        let url = AttachmentSummary::from_apple_events_metadata(
            AttachmentMetadata {
                id: AttachmentId::from("url"),
                note_id: NoteId::from("note"),
                display_name: "Website".into(),
                content_identifier: None,
                source_url: Some("https://example.invalid".into()),
                creation_date: date.clone(),
                modification_date: date,
                shared: false,
            },
            AttachmentCapabilities::notes_apple_events(),
        );
        assert_eq!(url.kind, AttachmentKind::Url);
        assert_eq!(
            url.preview_status,
            AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::UnsafeUrl)
        );
        assert_eq!(
            url.export_status,
            AttachmentAccessStatus::Unavailable(AttachmentUnavailableReason::UnsafeUrl)
        );
    }

    #[test]
    fn safe_html_escapes_and_preserves_empty_lines() {
        assert_eq!(
            plaintext_to_safe_html("<&\"\n\nтест"),
            r#"<div>&lt;&amp;&quot;</div>
<div></div>
<div>тест</div>"#
        );
    }

    #[test]
    fn editability_distinguishes_plain_rich_and_unsafe_notes() {
        let mut note = Note {
            summary: NoteSummary {
                id: NoteId::from("note"),
                folder_id: FolderId::from("folder"),
                name: "Sample".into(),
                creation_date: NoteDate::new("date"),
                modification_date: NoteDate::new("date"),
                password_protected: false,
                shared: false,
                attachment_count: 0,
            },
            account_id: AccountId::from("account"),
            body_html: "<div>plain</div>".into(),
            plaintext: "plain".into(),
            attachments: vec![],
        };
        assert_eq!(classify_editability(&note), Editability::PlainText);
        note.body_html = "<h1>Heading</h1><div><b>bold</b></div>".into();
        assert_eq!(classify_editability(&note), Editability::RichTextSupported);
        note.body_html = "<table><tr><td>x</td></tr></table>".into();
        assert!(matches!(
            classify_editability(&note),
            Editability::ReadOnlyUnsupported { .. }
        ));
        note.body_html = "<div>plain</div>".into();
        note.summary.attachment_count = 1;
        assert!(matches!(
            classify_editability(&note),
            Editability::ReadOnlyUnsupported { .. }
        ));
    }
}
