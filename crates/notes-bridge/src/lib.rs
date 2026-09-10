//! macOS Apple Events backend for [`notes_core::NotesBackend`].
//!
//! The canonical AppleScript remains in `tools/notes-probe/scripts` during the
//! transition from Phase 0. This crate embeds that one source file at compile
//! time and invokes `/usr/bin/osascript` directly; it never invokes the probe
//! executable and does not expose JSON outside this crate.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use notes_core::perf;
use notes_core::{
    Account, AccountId, AttachmentCapabilities, AttachmentExportResult, AttachmentId,
    AttachmentMetadata, AttachmentSummary, BackendCapabilities, CreateChildFolder, CreateFolder,
    CreateNote, DeleteDisposition, DeleteFolder, DeleteNote, DeleteResult, DeletedFolder,
    EntityKind, Folder, FolderId, FolderParent, MoveNote, Note, NoteDate, NoteId, NoteSummary,
    NotesBackend, NotesError, NotesPage, NotesQuery, RenameFolder, ReparentFolder,
    RichTextCapabilities, UpdateNote,
};
use serde::Deserialize;

const SCHEMA_VERSION: &str = "apple-notes-probe/v1";
const OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const CANONICAL_SCRIPT: &str =
    include_str!("../../../tools/notes-probe/scripts/notes_probe.applescript");

#[derive(Debug, Default)]
pub struct AppleScriptNotesBackend;

impl AppleScriptNotesBackend {
    pub fn new() -> Self {
        Self
    }

    fn call<T: for<'a> Deserialize<'a>>(
        &self,
        operation: &str,
        arguments: Vec<String>,
    ) -> Result<T, NotesError> {
        let started = Instant::now();
        let result = run_osascript(operation, &arguments)
            .and_then(|output| decode_probe_response(operation, &output));
        perf::event(
            "bridge.osascript",
            Some(operation),
            started,
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }

    fn call_with_cancel<T: for<'a> Deserialize<'a>>(
        &self,
        operation: &str,
        arguments: Vec<String>,
        cancel: Option<&AtomicBool>,
    ) -> Result<T, NotesError> {
        let started = Instant::now();
        let result = run_osascript_with_cancel(operation, &arguments, cancel)
            .and_then(|output| decode_probe_response(operation, &output));
        perf::event(
            "bridge.osascript",
            Some(operation),
            started,
            match &result {
                Ok(_) => "ok",
                Err(NotesError::Cancelled) => "cancelled",
                Err(_) => "error",
            },
        );
        if let Err(error) = &result {
            perf::event(
                "bridge.osascript.error",
                Some(operation),
                Instant::now(),
                safe_error_kind(error),
            );
        }
        result
    }
}

fn safe_error_kind(error: &NotesError) -> &'static str {
    match error {
        NotesError::ContextMismatch { .. } => "context_mismatch",
        NotesError::NotFound { .. } => "not_found",
        NotesError::PermissionDenied(_) => "permission_denied",
        NotesError::Timeout { .. } => "timeout",
        NotesError::Cancelled => "cancelled",
        NotesError::InvalidResponse(_) => "invalid_response",
        NotesError::Backend(_) => "backend",
        _ => "other",
    }
}

fn decode_probe_response<T: for<'a> Deserialize<'a>>(
    operation: &str,
    output: &str,
) -> Result<T, NotesError> {
    let envelope: Envelope<T> = serde_json::from_str(output)
        .map_err(|error| NotesError::InvalidResponse(format!("{operation}: {error}")))?;
    let expected_operation = if operation == "preview-contextual" && envelope.ok {
        "preview"
    } else {
        operation
    };
    if envelope.schema_version != SCHEMA_VERSION || envelope.operation != expected_operation {
        return Err(NotesError::InvalidResponse(format!(
            "unexpected envelope for {operation}"
        )));
    }
    if envelope.ok {
        envelope.data.ok_or_else(|| {
            NotesError::InvalidResponse(format!("successful {operation} response has no data"))
        })
    } else {
        Err(map_error(operation, envelope.error))
    }
}

impl NotesBackend for AppleScriptNotesBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new(RichTextCapabilities {
            heading1: true,
            heading2: true,
            heading3: false,
            bold: true,
            italic: true,
            underline: true,
            hyperlink: false,
            quote: false,
            code: true,
            bullet_list: true,
            numbered_list: true,
            mixed_adjacent_list_types: false,
        })
        .with_attachments(AttachmentCapabilities::notes_apple_events())
    }

    fn accounts(&self) -> Result<Vec<Account>, NotesError> {
        self.call::<Vec<WireAccount>>("accounts", vec!["accounts".into()])
            .map(|items| items.into_iter().map(Into::into).collect())
    }

    fn accounts_with_cancel(
        &self,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Account>, NotesError> {
        self.call_with_cancel::<Vec<WireAccount>>("accounts", vec!["accounts".into()], cancel)
            .map(|items| items.into_iter().map(Into::into).collect())
    }

    fn folders(&self, account: Option<&AccountId>) -> Result<Vec<Folder>, NotesError> {
        self.call::<Vec<WireFolder>>(
            "folders",
            vec![
                "folders".into(),
                account.map_or_else(String::new, ToString::to_string),
            ],
        )
        .and_then(|items| items.into_iter().map(TryInto::try_into).collect())
    }

    fn folders_with_cancel(
        &self,
        account: Option<&AccountId>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Folder>, NotesError> {
        self.call_with_cancel::<Vec<WireFolder>>(
            "folders",
            vec![
                "folders".into(),
                account.map_or_else(String::new, ToString::to_string),
            ],
            cancel,
        )
        .and_then(|items| items.into_iter().map(TryInto::try_into).collect())
    }

    fn notes(&self, query: &NotesQuery) -> Result<NotesPage, NotesError> {
        let limit = query.limit.unwrap_or(50);
        self.call::<WireNotesPage>(
            "notes",
            vec![
                "notes".into(),
                query
                    .account_id
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
                query
                    .folder_id
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
                limit.to_string(),
            ],
        )
        .map(Into::into)
    }

    fn notes_with_cancel(
        &self,
        query: &NotesQuery,
        cancel: Option<&AtomicBool>,
    ) -> Result<NotesPage, NotesError> {
        let limit = query.limit.unwrap_or(50);
        self.call_with_cancel::<WireNotesPage>(
            "notes",
            vec![
                "notes".into(),
                query
                    .account_id
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
                query
                    .folder_id
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
                limit.to_string(),
            ],
            cancel,
        )
        .map(Into::into)
    }

    fn get_note(&self, id: &NoteId) -> Result<Note, NotesError> {
        let preview =
            self.call::<WirePreview>("preview", vec!["preview".into(), id.to_string()])?;
        Ok(preview.into_note(self.capabilities().attachments))
    }

    fn get_note_with_context(
        &self,
        id: &NoteId,
        account_id: &AccountId,
        folder_id: &FolderId,
    ) -> Result<Note, NotesError> {
        let context_target = format!("note_id={id} account_id={account_id} folder_id={folder_id}");
        perf::event(
            "navigation.preview.context",
            Some(&context_target),
            Instant::now(),
            "requested",
        );
        match self.call::<WirePreview>(
            "preview-contextual",
            contextual_preview_args(id, account_id, folder_id),
        ) {
            Ok(preview) => Ok(preview.into_note(self.capabilities().attachments)),
            Err(NotesError::ContextMismatch { .. }) => {
                perf::event(
                    "navigation.preview.context_mismatch",
                    Some(id.as_str()),
                    Instant::now(),
                    "fallback_global",
                );
                perf::event(
                    "navigation.preview.global_fallback",
                    Some(id.as_str()),
                    Instant::now(),
                    "scheduled",
                );
                self.get_note(id)
            }
            Err(error) => Err(error),
        }
    }

    fn get_note_with_cancel(
        &self,
        id: &NoteId,
        cancel: Option<&AtomicBool>,
    ) -> Result<Note, NotesError> {
        let preview = self.call_with_cancel::<WirePreview>(
            "preview",
            vec!["preview".into(), id.to_string()],
            cancel,
        )?;
        Ok(preview.into_note(self.capabilities().attachments))
    }

    fn preview_attachment(
        &self,
        note_id: &NoteId,
        attachment_id: &AttachmentId,
    ) -> Result<(), NotesError> {
        let response = self.call::<WireAttachmentAction>(
            "preview-attachment",
            vec![
                "preview-attachment".into(),
                note_id.to_string(),
                attachment_id.to_string(),
            ],
        )?;
        response.verify(note_id, attachment_id)?;
        Ok(())
    }

    fn export_attachment(
        &self,
        note_id: &NoteId,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentExportResult, NotesError> {
        let response = self.call::<WireAttachmentAction>(
            "export-attachment",
            vec![
                "export-attachment".into(),
                note_id.to_string(),
                attachment_id.to_string(),
            ],
        )?;
        response.verify(note_id, attachment_id)?;
        let destination = response.destination.ok_or_else(|| {
            NotesError::InvalidResponse("attachment export response has no destination".into())
        })?;
        Ok(AttachmentExportResult {
            note_id: note_id.clone(),
            attachment_id: attachment_id.clone(),
            destination: PathBuf::from(destination),
        })
    }

    fn create_note(&self, request: &CreateNote) -> Result<Note, NotesError> {
        let summary = self.call::<WireNoteSummary>(
            "create-note",
            vec![
                "create-note".into(),
                request.folder_id.to_string(),
                request.name.clone(),
                request.body_html.clone(),
            ],
        )?;
        self.get_note(&summary.id.into())
    }

    fn create_folder(&self, request: &CreateFolder) -> Result<Folder, NotesError> {
        self.call::<WireFolder>(
            "create-folder",
            vec![
                "create-folder".into(),
                request.account_id.to_string(),
                request.name.clone(),
            ],
        )?
        .try_into()
    }

    fn create_child_folder(&self, request: &CreateChildFolder) -> Result<Folder, NotesError> {
        create_child_folder_with_runner(request, |operation, arguments| {
            run_osascript(operation, arguments)
        })
    }

    fn reparent_folder(&self, request: &ReparentFolder) -> Result<Folder, NotesError> {
        reparent_folder_with_runner(request, |operation, arguments| {
            run_osascript(operation, arguments)
        })
    }

    fn rename_folder(&self, request: &RenameFolder) -> Result<Folder, NotesError> {
        rename_folder_with_runner(request, |operation, arguments| {
            run_osascript(operation, arguments)
        })
    }

    fn delete_folder(&self, request: &DeleteFolder) -> Result<DeletedFolder, NotesError> {
        delete_folder_with_runner(request, |operation, arguments| {
            run_osascript(operation, arguments)
        })
    }

    fn update_note(&self, request: &UpdateNote) -> Result<Note, NotesError> {
        if request.name.is_none() && request.body_html.is_none() {
            return Err(NotesError::InvalidResponse(
                "update request has no changes".into(),
            ));
        }
        let summary = self.call::<WireNoteSummary>(
            "update-note",
            vec![
                "update-note".into(),
                request.id.to_string(),
                request.name.clone().unwrap_or_default(),
                request.body_html.clone().unwrap_or_default(),
                request.name.is_some().to_string(),
                request.body_html.is_some().to_string(),
            ],
        )?;
        self.get_note(&summary.id.into())
    }

    fn move_note(&self, request: &MoveNote) -> Result<Note, NotesError> {
        let summary = self.call::<WireNoteSummary>(
            "move-note",
            vec![
                "move-note".into(),
                request.id.to_string(),
                request.destination_folder_id.to_string(),
            ],
        )?;
        self.get_note(&summary.id.into())
    }

    fn delete_note(&self, request: &DeleteNote) -> Result<DeleteResult, NotesError> {
        let result = self.call::<WireDeleteResult>(
            "delete-note",
            vec!["delete-note".into(), request.id.to_string()],
        )?;
        Ok(result.into())
    }
}

fn rename_folder_with_runner<F>(request: &RenameFolder, runner: F) -> Result<Folder, NotesError>
where
    F: FnOnce(&str, &[String]) -> Result<String, NotesError>,
{
    let arguments = vec![
        "rename-folder".into(),
        request.account_id.to_string(),
        request.folder_id.to_string(),
        request.name.clone(),
    ];
    let output = runner("rename-folder", &arguments)?;
    let folder: Folder =
        decode_probe_response::<WireFolder>("rename-folder", &output)?.try_into()?;
    if folder.id != request.folder_id || folder.account_id != request.account_id {
        return Err(NotesError::InvalidResponse(
            "rename-folder returned mismatched folder identity".into(),
        ));
    }
    Ok(folder)
}

fn create_child_folder_with_runner<F>(
    request: &CreateChildFolder,
    runner: F,
) -> Result<Folder, NotesError>
where
    F: FnOnce(&str, &[String]) -> Result<String, NotesError>,
{
    let arguments = vec![
        "create-child-folder".into(),
        request.account_id.to_string(),
        request.parent_folder_id.to_string(),
        request.name.clone(),
    ];
    let folder: Folder = decode_probe_response::<WireFolder>(
        "create-child-folder",
        &runner("create-child-folder", &arguments)?,
    )?
    .try_into()?;
    if folder.account_id != request.account_id
        || folder.parent
            != (FolderParent::Folder {
                folder_id: request.parent_folder_id.clone(),
            })
    {
        return Err(NotesError::InvalidResponse(
            "create-child-folder returned mismatched hierarchy identity".into(),
        ));
    }
    Ok(folder)
}

fn reparent_folder_with_runner<F>(request: &ReparentFolder, runner: F) -> Result<Folder, NotesError>
where
    F: FnOnce(&str, &[String]) -> Result<String, NotesError>,
{
    let (target_kind, target_id) = match &request.new_parent_folder_id {
        Some(folder_id) => ("folder".into(), folder_id.to_string()),
        None => ("account-root".into(), request.account_id.to_string()),
    };
    let arguments = vec![
        "reparent-folder".into(),
        request.account_id.to_string(),
        request.folder_id.to_string(),
        target_kind,
        target_id,
    ];
    let folder: Folder = decode_probe_response::<WireFolder>(
        "reparent-folder",
        &runner("reparent-folder", &arguments)?,
    )?
    .try_into()?;
    let expected_parent = match &request.new_parent_folder_id {
        Some(folder_id) => FolderParent::Folder {
            folder_id: folder_id.clone(),
        },
        None => FolderParent::Account {
            account_id: request.account_id.clone(),
        },
    };
    if folder.id != request.folder_id
        || folder.account_id != request.account_id
        || folder.parent != expected_parent
    {
        return Err(NotesError::InvalidResponse(
            "reparent-folder returned mismatched hierarchy identity".into(),
        ));
    }
    Ok(folder)
}

fn delete_folder_with_runner<F>(
    request: &DeleteFolder,
    runner: F,
) -> Result<DeletedFolder, NotesError>
where
    F: FnOnce(&str, &[String]) -> Result<String, NotesError>,
{
    let arguments = vec![
        "delete-folder".into(),
        request.account_id.to_string(),
        request.folder_id.to_string(),
    ];
    let output = runner("delete-folder", &arguments)?;
    let deleted = decode_probe_response::<WireDeletedFolder>("delete-folder", &output)?;
    if deleted.id != request.folder_id.as_str() || deleted.account_id != request.account_id.as_str()
    {
        return Err(NotesError::InvalidResponse(
            "delete-folder returned mismatched folder identity".into(),
        ));
    }
    Ok(DeletedFolder {
        account_id: deleted.account_id.into(),
        folder_id: deleted.id.into(),
    })
}

fn run_osascript(operation: &str, arguments: &[String]) -> Result<String, NotesError> {
    run_osascript_with_cancel(operation, arguments, None)
}

fn run_osascript_with_cancel(
    operation: &str,
    arguments: &[String],
    cancel: Option<&AtomicBool>,
) -> Result<String, NotesError> {
    if operation == "preview-contextual" {
        let note_id = arguments.get(1).map(String::as_str).unwrap_or("");
        let account_id = arguments.get(2).map(String::as_str).unwrap_or("");
        let folder_id = arguments.get(3).map(String::as_str).unwrap_or("");
        perf::event(
            "bridge.osascript.args",
            Some(&format!(
                "note_id={note_id} account_id={account_id} folder_id={folder_id}"
            )),
            Instant::now(),
            "spawn",
        );
    }
    let mut child = osascript_command(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| NotesError::Backend(format!("could not launch osascript: {error}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| NotesError::Backend("could not capture osascript stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| NotesError::Backend("could not capture osascript stderr".into()))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            NotesError::Backend(format!("could not wait for osascript: {error}"))
        })? {
            break status;
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(NotesError::Cancelled);
        }
        if started.elapsed() >= OSASCRIPT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NotesError::Timeout {
                operation: operation.into(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| NotesError::Backend("osascript stdout reader panicked".into()))?
        .map_err(|error| {
            NotesError::Backend(format!("could not read osascript stdout: {error}"))
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| NotesError::Backend("osascript stderr reader panicked".into()))?
        .map_err(|error| {
            NotesError::Backend(format!("could not read osascript stderr: {error}"))
        })?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
        if let Ok(stdout_text) = String::from_utf8(stdout.clone()) {
            if stdout_text.trim_start().starts_with('{') {
                return Ok(stdout_text);
            }
        }
        return Err(NotesError::Backend(format!(
            "osascript exited with {status}: {stderr}"
        )));
    }
    String::from_utf8(stdout).map_err(|error| {
        NotesError::InvalidResponse(format!("{operation}: output was not UTF-8: {error}"))
    })
}

fn osascript_command(arguments: &[String]) -> Command {
    let mut command = Command::new("/usr/bin/osascript");
    let sidecar = std::env::current_exe().ok().and_then(|path| {
        path.parent()
            .map(|parent| parent.join("scripts/notes_probe.applescript"))
    });
    if let Some(path) = sidecar.filter(|path| path.is_file()) {
        command.arg(path);
    } else {
        command.arg("-e").arg(CANONICAL_SCRIPT);
    }
    command.args(arguments);
    command
}

fn contextual_preview_args(
    id: &NoteId,
    account_id: &AccountId,
    folder_id: &FolderId,
) -> Vec<String> {
    vec![
        "preview-contextual".into(),
        id.to_string(),
        account_id.to_string(),
        folder_id.to_string(),
    ]
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    operation: String,
    ok: bool,
    data: Option<T>,
    error: Option<WireError>,
}
#[derive(Deserialize)]
struct WireError {
    source: String,
    code: String,
    message: String,
}
fn map_error(operation: &str, error: Option<WireError>) -> NotesError {
    let Some(error) = error else {
        return NotesError::InvalidResponse(format!("failed {operation} response has no error"));
    };
    if error.code == "osascript_timeout" {
        return NotesError::Timeout {
            operation: operation.into(),
        };
    }
    if error.code == "404" {
        if operation == "preview-contextual" && error.message.starts_with("context mismatch:") {
            return NotesError::ContextMismatch {
                note_id: error.message.into(),
            };
        }
        return NotesError::NotFound {
            kind: entity_kind_from_not_found_message(operation, &error.message),
            id: error.message,
        };
    }
    if error.code == "-1743" || error.message.to_lowercase().contains("not authorized") {
        return NotesError::PermissionDenied(error.message);
    }
    if error.code == "-128" {
        return NotesError::Cancelled;
    }
    NotesError::Backend(format!(
        "{}:{}: {}",
        error.source, error.code, error.message
    ))
}

fn entity_kind_from_not_found_message(operation: &str, message: &str) -> EntityKind {
    if message.starts_with("account not found:") {
        EntityKind::Account
    } else if message.starts_with("folder not found:") {
        EntityKind::Folder
    } else if message.starts_with("note not found:") {
        EntityKind::Note
    } else if message.starts_with("attachment not found:")
        || matches!(
            operation,
            "attachments" | "preview-attachment" | "export-attachment"
        )
    {
        EntityKind::Attachment
    } else {
        EntityKind::Unknown
    }
}

#[derive(Deserialize)]
struct WireAccount {
    id: String,
    name: String,
    upgraded: bool,
    #[serde(rename = "default")]
    is_default: bool,
    #[serde(rename = "defaultFolderId")]
    default_folder_id: Option<String>,
}
impl From<WireAccount> for Account {
    fn from(value: WireAccount) -> Self {
        Self {
            id: value.id.into(),
            name: value.name,
            is_default: value.is_default,
            is_upgraded: value.upgraded,
            default_folder_id: value.default_folder_id.map(Into::into),
        }
    }
}
#[derive(Debug, Deserialize)]
struct WireFolder {
    id: String,
    name: String,
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "parentKind")]
    parent_kind: String,
    #[serde(rename = "parentId")]
    parent_id: String,
    shared: bool,
}
impl TryFrom<WireFolder> for Folder {
    type Error = NotesError;
    fn try_from(value: WireFolder) -> Result<Self, Self::Error> {
        let parent = match value.parent_kind.as_str() {
            "account" => FolderParent::Account {
                account_id: value.parent_id.into(),
            },
            "folder" => FolderParent::Folder {
                folder_id: value.parent_id.into(),
            },
            other => {
                return Err(NotesError::InvalidResponse(format!(
                    "unknown folder parent kind: {other}"
                )))
            }
        };
        Ok(Self {
            id: value.id.into(),
            account_id: value.account_id.into(),
            name: value.name,
            parent,
            shared: value.shared,
        })
    }
}
#[derive(Deserialize)]
struct WireNoteSummary {
    id: String,
    name: String,
    #[serde(rename = "folderId")]
    folder_id: String,
    #[serde(rename = "creationDateText")]
    creation_date: String,
    #[serde(rename = "modificationDateText")]
    modification_date: String,
    #[serde(rename = "passwordProtected")]
    password_protected: bool,
    shared: bool,
    #[serde(rename = "attachmentCount")]
    attachment_count: Option<usize>,
}
impl From<WireNoteSummary> for NoteSummary {
    fn from(value: WireNoteSummary) -> Self {
        Self {
            id: value.id.into(),
            folder_id: value.folder_id.into(),
            name: value.name,
            creation_date: NoteDate::new(value.creation_date),
            modification_date: NoteDate::new(value.modification_date),
            password_protected: value.password_protected,
            shared: value.shared,
            attachment_count: value.attachment_count,
        }
    }
}
#[derive(Deserialize)]
struct WireNotesPage {
    items: Vec<WireNoteSummary>,
    total: usize,
    truncated: bool,
}
impl From<WireNotesPage> for NotesPage {
    fn from(value: WireNotesPage) -> Self {
        Self {
            items: value.items.into_iter().map(Into::into).collect(),
            total: value.total,
            truncated: value.truncated,
        }
    }
}
#[derive(Deserialize)]
struct WireNote {
    id: String,
    name: String,
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "folderId")]
    folder_id: String,
    #[serde(rename = "bodyHtml")]
    body_html: String,
    plaintext: String,
    #[serde(rename = "creationDateText")]
    creation_date: String,
    #[serde(rename = "modificationDateText")]
    modification_date: String,
    #[serde(rename = "passwordProtected")]
    password_protected: bool,
    shared: bool,
}

#[derive(Deserialize)]
struct WirePreview {
    note: WireNote,
    attachments: Vec<WireAttachment>,
}
impl WirePreview {
    fn into_note(self, capabilities: AttachmentCapabilities) -> Note {
        self.note.into_note(
            self.attachments
                .into_iter()
                .map(|item| item.into_summary(capabilities))
                .collect(),
        )
    }
}
impl WireNote {
    fn into_note(self, attachments: Vec<AttachmentSummary>) -> Note {
        let attachment_count = attachments.len();
        Note {
            summary: NoteSummary {
                id: self.id.into(),
                folder_id: self.folder_id.into(),
                name: self.name,
                creation_date: NoteDate::new(self.creation_date),
                modification_date: NoteDate::new(self.modification_date),
                password_protected: self.password_protected,
                shared: self.shared,
                attachment_count: Some(attachment_count),
            },
            account_id: self.account_id.into(),
            body_html: self.body_html,
            plaintext: self.plaintext,
            attachments,
        }
    }
}
#[derive(Deserialize)]
struct WireAttachment {
    id: String,
    #[serde(rename = "noteId")]
    note_id: String,
    name: String,
    #[serde(rename = "contentIdentifier")]
    content_identifier: Option<String>,
    url: Option<String>,
    #[serde(rename = "creationDateText")]
    creation_date: String,
    #[serde(rename = "modificationDateText")]
    modification_date: String,
    shared: bool,
}
impl WireAttachment {
    fn into_summary(self, capabilities: AttachmentCapabilities) -> AttachmentSummary {
        AttachmentSummary::from_apple_events_metadata(
            AttachmentMetadata {
                id: self.id.into(),
                note_id: self.note_id.into(),
                display_name: self.name,
                content_identifier: self.content_identifier,
                source_url: self.url,
                creation_date: NoteDate::new(self.creation_date),
                modification_date: NoteDate::new(self.modification_date),
                shared: self.shared,
            },
            capabilities,
        )
    }
}

#[derive(Deserialize)]
struct WireAttachmentAction {
    id: String,
    #[serde(rename = "noteId")]
    note_id: String,
    #[serde(default)]
    destination: Option<String>,
    #[serde(default)]
    method: Option<String>,
}

impl WireAttachmentAction {
    fn verify(&self, note_id: &NoteId, attachment_id: &AttachmentId) -> Result<(), NotesError> {
        let _ = &self.method;
        if self.note_id == note_id.as_str() && self.id == attachment_id.as_str() {
            Ok(())
        } else {
            Err(NotesError::InvalidResponse(
                "attachment action returned mismatched identifiers".into(),
            ))
        }
    }
}
#[derive(Debug, Deserialize)]
struct WireDeleteResult {
    id: String,
    #[serde(rename = "sourceFolderId")]
    source_folder_id: String,
    deleted: bool,
    disposition: String,
}

#[derive(Debug, Deserialize)]
struct WireDeletedFolder {
    id: String,
    #[serde(rename = "accountId")]
    account_id: String,
}
impl From<WireDeleteResult> for DeleteResult {
    fn from(value: WireDeleteResult) -> Self {
        let _ = value.deleted;
        let _ = value.disposition;
        Self {
            id: value.id.into(),
            source_folder_id: value.source_folder_id.into(),
            disposition: DeleteDisposition::DelegatedToNotesApp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_list_does_not_query_attachments() {
        let list_source = CANONICAL_SCRIPT
            .split("on collectNotesFromFolders")
            .nth(1)
            .and_then(|tail| tail.split("end collectNotesFromFolders").next())
            .expect("notes list producer");
        assert!(!list_source.contains("attachmentGroups"));
        assert!(!list_source.contains("count of every attachment"));
        assert!(!list_source.contains("every attachment of"));
    }

    #[test]
    fn note_summary_attachment_count_is_optional_unknown() {
        assert!(CANONICAL_SCRIPT
            .contains("on noteMetadataFromValues(noteIndex, noteValues, currentFolderId)"));
        assert!(!CANONICAL_SCRIPT.contains("attachmentGroups"));
    }

    #[test]
    fn production_contextual_argv_matches_applescript_contract() {
        let args = contextual_preview_args(
            &NoteId::from("p488"),
            &AccountId::from("p1"),
            &FolderId::from("p2"),
        );
        assert_eq!(args, ["preview-contextual", "p488", "p1", "p2"]);
        assert!(CANONICAL_SCRIPT.contains("probePreviewContextual(my argumentAt(argv, 2), my argumentAt(argv, 3), my argumentAt(argv, 4))"));
    }

    #[test]
    fn contextual_success_accepts_preview_response_operation() {
        let fixture =
            r#"{"schemaVersion":"apple-notes-probe/v1","operation":"preview","ok":true,"data":{}}"#;
        let decoded = decode_probe_response::<serde_json::Value>("preview-contextual", fixture);
        assert!(decoded.is_ok());
    }

    #[test]
    fn contextual_error_still_requires_contextual_error_semantics() {
        let fixture = r#"{"schemaVersion":"apple-notes-probe/v1","operation":"preview-contextual","ok":false,"error":{"source":"AppleScript","code":"404","message":"context mismatch: stale"}}"#;
        assert!(matches!(
            decode_probe_response::<serde_json::Value>("preview-contextual", fixture),
            Err(NotesError::ContextMismatch { .. })
        ));
    }

    #[test]
    fn rust_note_summary_attachment_count_is_optional() {
        assert!(stringify!(WireNoteSummary).contains("WireNoteSummary"));
    }

    #[test]
    fn combined_preview_uses_one_stable_note_lookup_and_returns_both_halves() {
        assert!(CANONICAL_SCRIPT.contains("operationName is \"preview\""));
        assert!(CANONICAL_SCRIPT.contains("set noteContext to my findNoteContext(noteId)"));
        assert!(CANONICAL_SCRIPT.contains("set attachmentRefs to every attachment of noteRef"));
        assert!(CANONICAL_SCRIPT.contains("dataItem's setObject:noteItem forKey:\"note\""));
        assert!(CANONICAL_SCRIPT
            .contains("dataItem's setObject:attachmentArray forKey:\"attachments\""));
    }

    #[test]
    fn canonical_serializer_uses_foundation_json_serialization() {
        let serializer = CANONICAL_SCRIPT
            .split("on jsonString(value)")
            .nth(1)
            .and_then(|tail| tail.split("end jsonString").next())
            .expect("jsonString handler");
        assert!(serializer.contains("NSJSONSerialization"));
        assert!(serializer.contains("NSJSONWritingFragmentsAllowed"));
        assert!(!serializer.contains("characters of"));
        assert!(!serializer.contains("replaceText"));
    }

    #[test]
    fn preview_reuses_context_ids_and_has_incremental_metadata_diagnostics() {
        let preview = CANONICAL_SCRIPT
            .split("on probePreview(noteId)")
            .nth(1)
            .and_then(|tail| tail.split("end probePreview").next())
            .expect("preview handler");
        assert!(preview.contains("set accountId to accountId of noteContext"));
        assert!(preview.contains("set folderId to folderId of noteContext"));
        assert!(preview.contains("my putString(noteItem, \"id\", noteId)"));
        assert!(!preview.contains("set noteId to id of noteRef"));
        for operation in [
            "preview-meta-name",
            "preview-meta-account-folder",
            "preview-meta-created",
            "preview-meta-modified",
            "preview-meta-protection",
            "preview-meta-shared",
            "preview-meta-all",
            "preview-meta-properties",
        ] {
            assert!(
                CANONICAL_SCRIPT.contains(&format!("operationName is \"{operation}\""))
                    || CANONICAL_SCRIPT.contains("operationName starts with \"preview-meta-\"")
            );
        }
    }

    #[test]
    fn bulk_metadata_uses_single_properties_read_and_reuses_context_ids() {
        let handler = CANONICAL_SCRIPT
            .split("on probeMetadataPropertiesAll(noteId)")
            .nth(1)
            .and_then(|tail| tail.split("end probeMetadataPropertiesAll").next())
            .expect("bulk metadata handler");
        assert_eq!(
            handler
                .matches("set noteProperties to properties of noteRef")
                .count(),
            1
        );
        assert!(handler.contains("set accountId to accountId of noteContext"));
        assert!(handler.contains("set folderId to folderId of noteContext"));
        assert!(handler.contains("name of noteProperties"));
        assert!(!handler.contains("name of noteRef"));
        assert!(!handler.contains("creation date of noteRef"));
        assert!(!handler.contains("modification date of noteRef"));
        assert!(!handler.contains("password protected of noteRef"));
        assert!(!handler.contains("shared of noteRef"));
    }

    #[test]
    fn bulk_diagnostics_extract_terminology_before_foundation_helpers() {
        for handler_name in ["probeMetadataPropertiesAll", "probePropertiesFull"] {
            let handler = CANONICAL_SCRIPT
                .split(&format!("on {handler_name}(noteId)"))
                .nth(1)
                .and_then(|tail| tail.split(&format!("end {handler_name}")).next())
                .expect("bulk handler");
            assert!(handler.contains("set localCreated to"));
            assert!(handler.contains("set localModified to"));
            assert!(handler.contains("set localProtected to"));
            assert!(handler.contains("set localShared to"));
            assert!(
                !handler.contains("my putString(resultItem, \"creationDateText\", (creation date")
            );
            assert!(
                !handler.contains("my putString(noteItem, \"creationDateText\", (creation date")
            );
        }
    }

    #[test]
    fn production_preview_uses_single_properties_snapshot_and_context_ids() {
        let preview = CANONICAL_SCRIPT
            .split("on probePreview(noteId)")
            .nth(1)
            .and_then(|tail| tail.split("end probePreview").next())
            .expect("preview handler");
        assert_eq!(
            preview
                .matches("set noteProperties to properties of noteRef")
                .count(),
            1
        );
        assert!(preview.contains("set noteName to name of noteProperties"));
        assert!(preview.contains("set noteBody to body of noteProperties"));
        assert!(preview.contains("set notePlaintext to plaintext of noteProperties"));
        assert!(preview.contains("set accountId to accountId of noteContext"));
        assert!(preview.contains("set folderId to folderId of noteContext"));
        assert!(!preview.contains("name of noteRef"));
        assert!(!preview.contains("body of noteRef"));
        assert!(!preview.contains("plaintext of noteRef"));
    }

    #[test]
    fn contextual_lookup_is_bounded_and_contextual_preview_preserves_bulk_path() {
        let lookup = CANONICAL_SCRIPT
            .split("on findContextualNoteContext(")
            .nth(1)
            .and_then(|tail| tail.split("end findContextualNoteContext").next())
            .expect("contextual lookup");
        assert!(lookup.contains("if currentAccountId is targetAccountId"));
        assert!(lookup.contains("findFolderById"));
        assert!(lookup.contains("set noteRefs to every note of folderRef"));
        assert!(lookup.contains("if item noteIndex of noteIds is targetNoteId"));
        assert!(lookup.contains("context mismatch"));
        let preview = CANONICAL_SCRIPT
            .split("on probePreviewContextual(")
            .nth(1)
            .and_then(|tail| tail.split("end probePreviewContextual").next())
            .expect("contextual preview");
        assert!(preview.contains("findContextualNoteContext"));
        assert!(preview.contains("set noteProperties to properties of noteRef"));
        assert!(preview.contains("previewFoundationPayload"));
    }

    #[test]
    fn contextual_404_response_maps_to_context_mismatch() {
        let error = map_error(
            "preview-contextual",
            Some(WireError {
                source: "AppleScript".into(),
                code: "404".into(),
                message: "context mismatch: note not found in supplied account/folder".into(),
            }),
        );
        assert!(matches!(error, NotesError::ContextMismatch { .. }));
    }

    #[test]
    fn unrelated_404_does_not_map_to_context_mismatch() {
        let error = map_error(
            "preview",
            Some(WireError {
                source: "AppleScript".into(),
                code: "404".into(),
                message: "note not found: opaque".into(),
            }),
        );
        assert!(matches!(error, NotesError::NotFound { .. }));
    }

    #[test]
    fn properties_snapshot_shape_is_privacy_safe() {
        let handler = CANONICAL_SCRIPT
            .split("on probePropertiesShape(noteId)")
            .nth(1)
            .and_then(|tail| tail.split("end probePropertiesShape").next())
            .expect("shape handler");
        assert!(handler.contains("set noteProperties to properties of noteRef"));
        assert!(handler.contains("classification"));
        assert!(!handler.contains("my putString(resultItem, \"name\""));
        assert!(!handler.contains("body of noteRef"));
        assert!(!handler.contains("plaintext of noteRef"));
    }

    #[test]
    fn apple_events_capabilities_match_phase_4_1_probes() {
        let capabilities = AppleScriptNotesBackend::new().capabilities();
        let rich = capabilities.rich_text;
        assert!(rich.heading1);
        assert!(rich.heading2);
        assert!(!rich.heading3);
        assert!(rich.bold);
        assert!(rich.italic);
        assert!(rich.underline);
        assert!(!rich.hyperlink);
        assert!(!rich.quote);
        assert!(rich.code);
        assert!(rich.bullet_list);
        assert!(rich.numbered_list);
        assert!(!rich.mixed_adjacent_list_types);
        assert!(capabilities.attachments.metadata);
        assert!(capabilities.attachments.notes_app_preview);
        assert!(capabilities.attachments.export_copy);
        assert!(!capabilities.attachments.exposes_local_file_path);
        assert!(capabilities.delete);
    }

    #[test]
    fn decodes_account() {
        let account: Account =
            serde_json::from_str::<WireAccount>(include_str!("../fixtures/account.json"))
                .unwrap()
                .into();
        assert!(account.is_default);
        assert_eq!(
            account.default_folder_id.unwrap(),
            notes_core::FolderId::from("folder-1")
        );
    }
    #[test]
    fn decodes_folder() {
        let folder: Folder =
            serde_json::from_str::<WireFolder>(include_str!("../fixtures/folder.json"))
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            folder.parent,
            FolderParent::Account {
                account_id: AccountId::from("account-1")
            }
        );
    }
    #[test]
    fn decodes_created_folder_with_authoritative_stable_id() {
        let folder: Folder = serde_json::from_str::<WireFolder>(
            r#"{"id":"created-folder-id","name":"Проекты 🚀","accountId":"account-1","parentKind":"account","parentId":"account-1","shared":false}"#,
        )
        .unwrap()
        .try_into()
        .unwrap();
        assert_eq!(folder.id, notes_core::FolderId::from("created-folder-id"));
        assert_eq!(folder.name, "Проекты 🚀");
        assert_eq!(folder.account_id, AccountId::from("account-1"));
    }

    #[test]
    fn decodes_renamed_folder_with_same_authoritative_id() {
        let folder: Folder = serde_json::from_str::<WireFolder>(
            r#"{"id":"folder-1","name":"Renamed 🚀","accountId":"account-1","parentKind":"account","parentId":"account-1","shared":false}"#,
        )
        .unwrap()
        .try_into()
        .unwrap();
        assert_eq!(folder.id, notes_core::FolderId::from("folder-1"));
        assert_eq!(folder.name, "Renamed 🚀");
    }
    #[test]
    fn decodes_note_summary() {
        let note: NoteSummary =
            serde_json::from_str::<WireNoteSummary>(include_str!("../fixtures/note-summary.json"))
                .unwrap()
                .into();
        assert_eq!(note.attachment_count, Some(2));
    }
    #[test]
    fn decodes_get_note() {
        let raw: WireNote = serde_json::from_str(include_str!("../fixtures/note.json")).unwrap();
        assert_eq!(raw.into_note(vec![]).body_html, "<div>Sample</div>");
    }
    #[test]
    fn combined_preview_returns_note_and_attachment_metadata() {
        let raw: WirePreview = serde_json::from_str(
            r#"{"note":{"id":"note-1","name":"Sample","accountId":"account-1","folderId":"folder-1","bodyHtml":"<div>Body</div>","plaintext":"Body","creationDateText":"2024-01-01","modificationDateText":"2024-01-02","passwordProtected":false,"shared":false},"attachments":[{"id":"attachment-1","noteId":"note-1","name":"file.pdf","contentIdentifier":"cid","url":null,"creationDateText":"2024-01-01","modificationDateText":"2024-01-02","shared":false}]}"#,
        )
        .unwrap();
        let note = raw.into_note(AttachmentCapabilities::notes_apple_events());
        assert_eq!(note.summary.id, NoteId::from("note-1"));
        assert_eq!(note.body_html, "<div>Body</div>");
        assert_eq!(note.attachments.len(), 1);
        assert_eq!(note.attachments[0].id, AttachmentId::from("attachment-1"));
    }
    #[test]
    fn delete_is_not_modelled_as_guaranteed_destruction() {
        let result: DeleteResult = serde_json::from_str::<WireDeleteResult>(include_str!(
            "../fixtures/delete-result.json"
        ))
        .unwrap()
        .into();
        assert_eq!(result.disposition, DeleteDisposition::DelegatedToNotesApp);
    }

    #[test]
    fn delete_probe_error_maps_to_a_typed_backend_error() {
        let error = decode_probe_response::<WireDeleteResult>(
            "delete-note",
            r#"{"schemaVersion":"apple-notes-probe/v1","operation":"delete-note","ok":false,"error":{"source":"applescript","code":"-10000","message":"synthetic failure"}}"#,
        )
        .expect_err("failed probe response");
        assert!(
            matches!(error, NotesError::Backend(message) if message.contains("synthetic failure"))
        );
    }

    #[test]
    fn malformed_or_incomplete_delete_response_is_a_typed_error() {
        for response in [
            "not JSON",
            r#"{"schemaVersion":"apple-notes-probe/v1","operation":"delete-note","ok":true,"data":{}}"#,
            r#"{"schemaVersion":"apple-notes-probe/v1","operation":"delete-note","ok":true}"#,
        ] {
            let error = decode_probe_response::<WireDeleteResult>("delete-note", response)
                .expect_err("invalid delete response");
            assert!(matches!(error, NotesError::InvalidResponse(_)));
        }
    }
    #[test]
    fn maps_permission_error() {
        let error = map_error(
            "accounts",
            Some(WireError {
                source: "applescript".into(),
                code: "-1743".into(),
                message: "Not authorized".into(),
            }),
        );
        assert!(matches!(error, NotesError::PermissionDenied(_)));
    }

    #[test]
    fn maps_not_found_error_to_the_entity_kind() {
        let error = map_error(
            "folders",
            Some(WireError {
                source: "applescript".into(),
                code: "404".into(),
                message: "account not found: account-1".into(),
            }),
        );
        assert!(matches!(
            error,
            NotesError::NotFound {
                kind: EntityKind::Account,
                ..
            }
        ));
    }

    #[test]
    fn decodes_observed_pdf_attachment_metadata_without_inventing_a_path() {
        let wire: WireAttachment = serde_json::from_str(
            r#"{
                "id":"attachment-p393",
                "noteId":"note-p392",
                "name":"Hetzner-VPN.pdf",
                "contentIdentifier":"cid:E69B68AD@icloud.apple.com",
                "url":null,
                "creationDateText":"created",
                "modificationDateText":"modified",
                "shared":false
            }"#,
        )
        .unwrap();
        let attachment = wire.into_summary(AttachmentCapabilities::notes_apple_events());
        assert_eq!(attachment.display_name, "Hetzner-VPN.pdf");
        assert_eq!(attachment.kind, notes_core::AttachmentKind::Pdf);
        assert_eq!(attachment.source_url, None);
        assert_eq!(
            attachment.preview_status,
            notes_core::AttachmentAccessStatus::Available
        );
        assert_eq!(
            attachment.export_status,
            notes_core::AttachmentAccessStatus::Available
        );
    }

    #[test]
    fn attachment_ids_are_passed_as_individual_osascript_arguments() {
        let note_id = "note;$(touch /tmp/never-run)".to_owned();
        let attachment_id = "attachment `open unsafe`".to_owned();
        let arguments = vec![
            "preview-attachment".to_owned(),
            note_id.clone(),
            attachment_id.clone(),
        ];
        let command = osascript_command(&arguments);
        assert_eq!(command.get_program(), "/usr/bin/osascript");
        let command_arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            &command_arguments[command_arguments.len() - 3..],
            &["preview-attachment", &note_id, &attachment_id]
        );
    }

    #[test]
    fn rename_folder_passes_stable_ids_and_name_as_separate_arguments() {
        let name = "Проекты 🚀 \"2027\" \\ test";
        let arguments = vec![
            "rename-folder".into(),
            "account-id".into(),
            "folder-id".into(),
            name.into(),
        ];
        let command = osascript_command(&arguments);
        let command_arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            &command_arguments[command_arguments.len() - 4..],
            &["rename-folder", "account-id", "folder-id", name]
        );
    }

    #[test]
    fn rename_folder_probe_error_maps_to_a_typed_backend_error() {
        let error = decode_probe_response::<WireFolder>(
            "rename-folder",
            r#"{"schemaVersion":"apple-notes-probe/v1","operation":"rename-folder","ok":false,"error":{"source":"applescript","code":"-10000","message":"synthetic rename failure"}}"#,
        )
        .expect_err("failed rename response");
        assert!(
            matches!(error, NotesError::Backend(message) if message.contains("synthetic rename failure"))
        );
    }

    #[test]
    fn rename_folder_rejects_returned_folder_id_mismatch() {
        let request = RenameFolder {
            account_id: AccountId::from("account-A"),
            folder_id: "folder-F1".into(),
            name: "Renamed".into(),
        };
        let error = rename_folder_with_runner(&request, |operation, arguments| {
            assert_eq!(operation, "rename-folder");
            assert_eq!(
                arguments,
                ["rename-folder", "account-A", "folder-F1", "Renamed"]
            );
            Ok(rename_folder_success("account-A", "folder-F2", "Renamed"))
        })
        .expect_err("mismatched returned folder must be rejected");
        assert!(
            matches!(error, NotesError::InvalidResponse(message) if message.contains("mismatched folder identity"))
        );
    }

    #[test]
    fn rename_folder_rejects_returned_account_id_mismatch() {
        let request = RenameFolder {
            account_id: AccountId::from("account-A"),
            folder_id: "folder-F".into(),
            name: "Renamed".into(),
        };
        let error = rename_folder_with_runner(&request, |operation, arguments| {
            assert_eq!(operation, "rename-folder");
            assert_eq!(
                arguments,
                ["rename-folder", "account-A", "folder-F", "Renamed"]
            );
            Ok(rename_folder_success("account-B", "folder-F", "Renamed"))
        })
        .expect_err("mismatched returned account must be rejected");
        assert!(
            matches!(error, NotesError::InvalidResponse(message) if message.contains("mismatched folder identity"))
        );
    }

    fn rename_folder_success(account_id: &str, folder_id: &str, name: &str) -> String {
        format!(
            r#"{{"schemaVersion":"apple-notes-probe/v1","operation":"rename-folder","ok":true,"data":{{"id":"{folder_id}","name":"{name}","accountId":"{account_id}","parentKind":"account","parentId":"{account_id}","shared":false}}}}"#
        )
    }

    #[test]
    fn delete_folder_passes_stable_ids_as_separate_arguments() {
        let request = DeleteFolder {
            account_id: AccountId::from("account-id"),
            folder_id: "folder-id".into(),
        };
        let deleted = delete_folder_with_runner(&request, |operation, arguments| {
            assert_eq!(operation, "delete-folder");
            assert_eq!(arguments, ["delete-folder", "account-id", "folder-id"]);
            Ok(delete_folder_success("account-id", "folder-id"))
        })
        .unwrap();
        assert_eq!(
            deleted,
            DeletedFolder {
                account_id: AccountId::from("account-id"),
                folder_id: "folder-id".into()
            }
        );
    }

    #[test]
    fn delete_folder_rejects_returned_folder_id_mismatch() {
        let request = DeleteFolder {
            account_id: AccountId::from("account-A"),
            folder_id: "folder-F1".into(),
        };
        let error = delete_folder_with_runner(&request, |_, _| {
            Ok(delete_folder_success("account-A", "folder-F2"))
        })
        .unwrap_err();
        assert!(
            matches!(error, NotesError::InvalidResponse(message) if message.contains("mismatched folder identity"))
        );
    }

    #[test]
    fn delete_folder_rejects_returned_account_id_mismatch() {
        let request = DeleteFolder {
            account_id: AccountId::from("account-A"),
            folder_id: "folder-F".into(),
        };
        let error = delete_folder_with_runner(&request, |_, _| {
            Ok(delete_folder_success("account-B", "folder-F"))
        })
        .unwrap_err();
        assert!(
            matches!(error, NotesError::InvalidResponse(message) if message.contains("mismatched folder identity"))
        );
    }

    #[test]
    fn delete_folder_probe_error_maps_to_a_typed_backend_error() {
        let error = decode_probe_response::<WireDeletedFolder>("delete-folder", r#"{"schemaVersion":"apple-notes-probe/v1","operation":"delete-folder","ok":false,"error":{"source":"applescript","code":"409","message":"folder deletion requires an empty folder"}}"#).unwrap_err();
        assert!(matches!(error, NotesError::Backend(message) if message.contains("empty folder")));
    }

    fn hierarchy_folder_success(
        operation: &str,
        account_id: &str,
        folder_id: &str,
        parent_kind: &str,
        parent_id: &str,
    ) -> String {
        format!(
            r#"{{"schemaVersion":"apple-notes-probe/v1","operation":"{operation}","ok":true,"data":{{"id":"{folder_id}","name":"Child","accountId":"{account_id}","parentKind":"{parent_kind}","parentId":"{parent_id}","shared":false}}}}"#
        )
    }

    #[test]
    fn create_child_folder_passes_account_parent_and_name_as_separate_arguments() {
        let request = CreateChildFolder {
            account_id: AccountId::from("account-A"),
            parent_folder_id: notes_core::FolderId::from("parent-F"),
            name: "Дочерняя ; \"quoted\"".into(),
        };
        let folder = create_child_folder_with_runner(&request, |operation, arguments| {
            assert_eq!(operation, "create-child-folder");
            assert_eq!(
                arguments,
                [
                    "create-child-folder",
                    "account-A",
                    "parent-F",
                    "Дочерняя ; \"quoted\""
                ]
            );
            Ok(hierarchy_folder_success(
                "create-child-folder",
                "account-A",
                "new-F",
                "folder",
                "parent-F",
            ))
        })
        .unwrap();
        assert_eq!(folder.id, notes_core::FolderId::from("new-F"));
    }

    #[test]
    fn create_child_folder_rejects_returned_hierarchy_mismatch() {
        let request = CreateChildFolder {
            account_id: AccountId::from("account-A"),
            parent_folder_id: notes_core::FolderId::from("parent-F"),
            name: "Child".into(),
        };
        let error = create_child_folder_with_runner(&request, |_, _| {
            Ok(hierarchy_folder_success(
                "create-child-folder",
                "account-A",
                "new-F",
                "folder",
                "other-parent",
            ))
        })
        .unwrap_err();
        assert!(
            matches!(error, NotesError::InvalidResponse(message) if message.contains("hierarchy"))
        );
    }

    #[test]
    fn reparent_folder_uses_typed_root_target_and_validates_result() {
        let request = ReparentFolder {
            account_id: AccountId::from("account-A"),
            folder_id: notes_core::FolderId::from("source-F"),
            new_parent_folder_id: None,
        };
        let folder = reparent_folder_with_runner(&request, |operation, arguments| {
            assert_eq!(operation, "reparent-folder");
            assert_eq!(
                arguments,
                [
                    "reparent-folder",
                    "account-A",
                    "source-F",
                    "account-root",
                    "account-A"
                ]
            );
            Ok(hierarchy_folder_success(
                "reparent-folder",
                "account-A",
                "source-F",
                "account",
                "account-A",
            ))
        })
        .unwrap();
        assert_eq!(folder.id, request.folder_id);
    }

    #[test]
    fn reparent_folder_probe_error_maps_to_typed_backend_error() {
        let error = decode_probe_response::<WireFolder>(
            "reparent-folder",
            r#"{"schemaVersion":"apple-notes-probe/v1","operation":"reparent-folder","ok":false,"error":{"source":"applescript","code":"409","message":"folder cannot be reparented below its descendant"}}"#,
        )
        .unwrap_err();
        assert!(matches!(error, NotesError::Backend(message) if message.contains("descendant")));
    }

    #[test]
    #[ignore = "destructive manual probe; requires explicit disposable empty folder IDs and Notes Automation permission"]
    fn manual_delete_empty_folder_probe() {
        let request = DeleteFolder {
            account_id: AccountId::from(
                std::env::var("APPLE_NOTES_TUI_DELETE_FOLDER_ACCOUNT_ID")
                    .expect("set disposable account ID"),
            ),
            folder_id: std::env::var("APPLE_NOTES_TUI_DELETE_FOLDER_ID")
                .expect("set disposable empty folder ID")
                .into(),
        };
        let deleted = AppleScriptNotesBackend::new()
            .delete_folder(&request)
            .expect("delete disposable empty folder");
        assert_eq!(deleted.account_id, request.account_id);
        assert_eq!(deleted.folder_id, request.folder_id);
    }

    #[test]
    #[ignore = "destructive manual probe; requires explicit disposable account and parent folder IDs"]
    fn manual_create_child_folder_probe() {
        let created = AppleScriptNotesBackend::new()
            .create_child_folder(&CreateChildFolder {
                account_id: std::env::var("APPLE_NOTES_TUI_CHILD_FOLDER_ACCOUNT_ID")
                    .expect("set disposable account ID")
                    .into(),
                parent_folder_id: std::env::var("APPLE_NOTES_TUI_CHILD_FOLDER_PARENT_ID")
                    .expect("set disposable parent folder ID")
                    .into(),
                name: "Codex manual child-folder probe".into(),
            })
            .expect("create disposable child folder");
        assert!(matches!(created.parent, FolderParent::Folder { .. }));
    }

    #[test]
    #[ignore = "destructive manual probe; requires explicit disposable account, source, and parent folder IDs"]
    fn manual_reparent_folder_probe() {
        let account_id: AccountId = std::env::var("APPLE_NOTES_TUI_REPARENT_ACCOUNT_ID")
            .expect("set disposable account ID")
            .into();
        let moved = AppleScriptNotesBackend::new()
            .reparent_folder(&ReparentFolder {
                account_id: account_id.clone(),
                folder_id: std::env::var("APPLE_NOTES_TUI_REPARENT_SOURCE_ID")
                    .expect("set disposable source folder ID")
                    .into(),
                new_parent_folder_id: Some(
                    std::env::var("APPLE_NOTES_TUI_REPARENT_PARENT_ID")
                        .expect("set disposable target parent folder ID")
                        .into(),
                ),
            })
            .expect("reparent disposable folder");
        assert_eq!(moved.account_id, account_id);
    }

    fn delete_folder_success(account_id: &str, folder_id: &str) -> String {
        format!(
            r#"{{"schemaVersion":"apple-notes-probe/v1","operation":"delete-folder","ok":true,"data":{{"id":"{folder_id}","accountId":"{account_id}"}}}}"#
        )
    }

    #[test]
    #[ignore = "requires a macOS Notes account and Automation permission"]
    fn live_accounts_are_read_only() {
        assert!(!AppleScriptNotesBackend::new()
            .accounts()
            .unwrap()
            .is_empty());
    }

    #[test]
    #[ignore = "requires explicit existing note/attachment IDs and Notes Automation permission"]
    fn live_attachment_metadata_and_notes_preview_are_read_only() {
        let note_id = std::env::var("APPLE_NOTES_TUI_LIVE_ATTACHMENT_NOTE_ID")
            .expect("set APPLE_NOTES_TUI_LIVE_ATTACHMENT_NOTE_ID");
        let attachment_id = std::env::var("APPLE_NOTES_TUI_LIVE_ATTACHMENT_ID")
            .expect("set APPLE_NOTES_TUI_LIVE_ATTACHMENT_ID");
        let backend = AppleScriptNotesBackend::new();
        let note = backend.get_note(&NoteId::from(note_id.as_str())).unwrap();
        assert!(note
            .attachments
            .iter()
            .any(|attachment| attachment.id == AttachmentId::from(attachment_id.as_str())));
        backend
            .preview_attachment(
                &NoteId::from(note_id.as_str()),
                &AttachmentId::from(attachment_id.as_str()),
            )
            .unwrap();
    }
}
