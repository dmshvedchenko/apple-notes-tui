# apple-notes-tui

A Rust-oriented terminal frontend for Apple Notes on macOS.

Apple Notes is the source of truth. This project does not read from or write to
NoteStore.sqlite.

## Architecture

```text
notes-core
    ↑
notes-bridge (Apple Events / AppleScript)
    ↑
future TUI

tools/notes-probe = diagnostics and integration testing
```

- `crates/notes-core` is the platform-neutral domain layer: typed IDs, models,
  requests, errors, and the `NotesBackend` trait.
- `crates/notes-bridge` implements that trait by launching `osascript`
  directly and decoding the JSON protocol internally. It does not invoke the
  probe executable.
- `tools/notes-probe` remains the Phase 0 diagnostic CLI and live integration
  harness; it is not an application-layer dependency.

## Project rules

1. Notes.app is the source of truth.
2. Never read or write `NoteStore.sqlite` directly.
3. The current production backend uses Apple Events/AppleScript.
4. The future TUI depends only on `NotesBackend` from `notes-core`.
5. GUI-only features may use a later helper/backend without changing the
   domain layer or TUI.

The current canonical AppleScript is
`tools/notes-probe/scripts/notes_probe.applescript`. During this transition it
is embedded by `notes-bridge` at compile time and read as a runtime file by the
diagnostic CLI. There is one script source, no production dependency on the
probe binary, and no duplicate AppleScript implementation.

## License

Licensed under the MIT License. See [LICENSE](LICENSE).

## macOS release use

The currently supported distribution model is a source release build on macOS:

~~~sh
cargo build --workspace --release --offline
./tools/notes-probe/target/release/apple-notes-tui --version
~~~

The configured Cargo target directory is `tools/notes-probe/target`; the
user-facing artifact is `apple-notes-tui`. `notes-probe` is a developer
diagnostic tool, not a required companion for the TUI. The TUI links
`notes-bridge`, whose canonical AppleScript is embedded at compile time, so the
installed TUI has no runtime dependency on the repository, a sibling helper,
or the diagnostic script file. It requires macOS, `/usr/bin/osascript`, and
the user's normal Notes automation permission. Start it from any working
directory.

No Homebrew formula, installer, or published crates.io package is provided at
present (`publish = false`). Release candidates should be built and smoke-tested
from the target macOS architecture before tagging.

All application-local files stay under
`~/Library/Application Support/apple-notes-tui/`: derived
`cache.sqlite3` (plus SQLite WAL files), `config.toml`, `session.toml`, and the
optional `editor-draft.json`. The cache and session are disposable and
non-authoritative. A recovery draft can contain unsaved note text; use
`--draft-info` for metadata-only inspection and `--draft-clear` for explicit
local removal. `--help`, `--version`, `--config-info`, `--cache-info`, and the
draft commands exit before terminal setup and do not contact Notes.app;
`--cache-clear` changes only the derived local cache. None accesses
`NoteStore.sqlite`.

## Phase 2: read-only terminal UI

Run without Notes.app using only synthetic data:

~~~sh
cargo run -p notes-tui -- --demo
~~~

Run against the configured Notes.app account (read-only):

~~~sh
cargo run -p notes-tui
~~~

`apple-notes-tui` presents accounts/folders, a list of note summaries, and a
plaintext preview. It loads a full note only when that note becomes selected.

Controls: `q` quit; `Tab`/`Shift-Tab` or `h`/`l` change pane; `j`/`k` move;
`D` move the selected note to Recently Deleted after confirmation (`y`/`Y`);
`n`/`N`/`Esc` cancels.

Deletion is delegated to Notes.app. `D` alone never mutates a note, backend
failures do not remove it locally, and permanent deletion or emptying Recently
Deleted is intentionally unsupported.
`g`/`G` first/last; `Enter` activate; `r` refresh; `?` help; and
`PgUp`/`PgDn` or `Ctrl-u`/`Ctrl-d` scroll the preview. `a` opens the selected
note's attachment list.

## Phase 7: in-memory title search

Press `/` in normal mode to edit a search query; `Enter` applies it,
`Backspace` edits it, `Ctrl-u` clears the input, and `Esc` cancels input or
clears an active filter. Search is case-insensitive and Unicode-safe, and is
currently **title-only**: it matches the loaded `NoteSummary.name`, not note
bodies or attachments. It therefore makes no `get_note` calls while typing or
applying a query.

The filter is scoped to the current loaded account/folder note set; there is no
global cross-account search. Results are stable `NoteId` references into the
authoritative runtime `notes` collection, not a second note store. Existing
selection is retained when it still matches; otherwise the first match is
selected, and zero matches have no selection. `Esc` restores the complete
loaded list. Folder/account navigation clears an active filter, while refresh
recomputes it against the reloaded list. Create, save, move, and delegated
delete refresh the authoritative list and then recompute any active filter.

Search never opens over the editor or an authoritative popup. It uses only the
existing `NotesBackend` runtime data: there is no NoteStore access, persistent
index, background synchronization, private API, or fuzzy ranking.

## Phase 3: safe simple-note editing

The TUI now supports `n` (new), `e` (edit), `Ctrl-s` (save), `Ctrl-c`
(discard an edit), and `m` (move). `Esc` leaves INSERT mode while retaining
the buffer. Dirty buffers are protected before quit or navigation.

The editor works with plaintext and serializes it to escaped `<div>` HTML.
Notes with attachments or unsupported rich markup are deliberately read-only
to prevent data loss. Before updating, the TUI reads the note again and checks
its modification date; on conflict it offers reload, overwrite, or cancel.
Delete remains intentionally out of scope.

## Phase 4: target-aware rich editor

The editor now keeps one typed `RichDocument` as the body source of truth. Each
cursor belongs to one visual target: a paragraph/heading/quote/code block or an
individual bullet/numbered-list item. Text input, Unicode-safe cursor movement,
Backspace/Delete, Enter, formatting, links, structural conversion, list
split/merge, and save all operate through that target.

While editing the body, `Alt-j`/`Alt-k` move between visual targets;
`Ctrl-b`/`Ctrl-i`/`Ctrl-u` toggle whole-target styles (`Alt-i` is an italic
fallback for terminals that encode `Ctrl-i` as Tab); `Ctrl-k` applies a link;
and `Alt-1`/`Alt-2`/`Alt-3`/`Alt-p`/`Alt-b`/`Alt-n`/`Alt-q`/`Alt-c` convert the
current target. `Ctrl-d` removes only the current visual target and always
leaves an editable document. It does not delete the Apple Notes note.

Formatting still applies to a complete visual target rather than an arbitrary
selection. Nested lists, checklists, tables, attachments, drawings, and scans
remain read-only or unsupported for editing.

### Phase 4.1: backend capability and lossless-save safety

`NotesBackend::capabilities()` is a cheap, static API. The editor analyzes its
typed `RichDocument` before every create and update and rejects the save with a
typed `UnsupportedRichContent` error when the active backend cannot preserve a
feature. The preflight also protects an existing note after unrelated edits:
unsupported content must be removed or converted before any backend mutation
can occur.

The Apple Events keyboard layer disables `Ctrl-k`, `Alt-q`, and `Alt-3` because
hyperlinks, quotes, and H3 are lossy. H2 remains enabled. Bullet and numbered
lists remain independently enabled; a conversion is blocked early only when
it would introduce a directly adjacent UL/OL or OL/UL boundary. The save
preflight remains the final safety check. Mock/demo advertises the complete
current editor subset and keeps every rich shortcut enabled.

| Feature | Apple Events backend | Mock/demo backend |
| --- | --- | --- |
| H1 | Supported with Notes normalization (bold 24 px span) | Supported |
| H2 | Supported with Notes normalization (bold 18 px span) | Supported |
| H3 | Unsupported-lossy (becomes ordinary bold) | Supported |
| Bold | Supported | Supported |
| Italic | Supported | Supported |
| Underline | Supported | Supported |
| Hyperlink | Unsupported-lossy (`href` is removed) | Supported |
| Quote | Unsupported-lossy (becomes an ordinary paragraph) | Supported |
| Code | Supported with Notes normalization (Courier `font`/`tt`) | Supported |
| Bullet list | Supported | Supported |
| Numbered list | Supported | Supported |
| Directly adjacent mixed list types | Unsupported-lossy (UL + OL becomes one UL) | Supported |

## Phase 5: read-only attachments

The preview shows the attachment count plus each attachment's display name,
derived kind, and preview availability. Press `a` to open the attachment
popup, `j`/`k` to select, `Enter` to preview explicitly in Notes.app, `x` to
export a copy through the macOS Save dialog, and `Esc` to close. Selection alone
never opens an attachment.

The Apple Events backend uses only the Notes scripting dictionary's `show` and
`save` commands. IDs are separate `osascript` arguments; attachment names are
displayed but never executed or interpolated into a shell command. No `open`,
`qlmanage`, temporary extraction, Accessibility automation, or NoteStore access
is used. Export is a copy to the destination explicitly chosen in the system
Save dialog; Notes.app remains the source of truth.

Notes opens attachment preview as a modal UI. Close that preview in Notes.app
before returning to the TUI; while it remains open, other synchronous Notes
Apple Events calls can wait until the backend timeout.

| Attachment capability | Apple Events backend | Mock/demo |
| --- | --- | --- |
| Stable attachment ID, parent note ID, display name | Available | Available |
| Content identifier, dates, shared flag | Available | Deterministic synthetic values |
| Attachment kind | Derived conservatively from filename extension; URL attachments are distinct | Image, PDF, and unknown fixtures |
| UTI / MIME / byte size | Not exposed by Notes AppleScript | Not invented |
| Local file URL/path | Not exposed for file attachments | Not invented |
| Explicit preview/open | Notes-native modal `show attachment`; known file kinds only | Simulated by injected backend |
| Export copy | Notes-native `save attachment in file` with macOS Save dialog | Simulated by injected backend |
| URL attachments | Metadata-only; non-file URLs are never opened | Modeled/tested as metadata-only |
| Unknown kinds | Metadata and export copy; preview unavailable | Covered by fixture/test |
| Password-protected notes | Attachment list/preview/export blocked with typed status | Covered by locked fixture/test |

File attachment `URL` values were `null` in every real Phase 5 probe. The
dictionary documents that property only for URL attachments and deliberately
hides the private attachment contents path, so Phase 5 does not expose a local
path or use Quick Look directly. Notes containing attachments remain read-only
for body edits under the existing lossless-save policy.

Real Notes.app verification on 2026-08-27 created and reopened two controlled
fixtures only in `Apple Notes TUI Test`. Heading, bold, italic, underline,
Unicode, emoji, and code survived create/read/update; one rich paragraph and
one bullet item were then changed independently without changing neighboring
content or order. Notes.app normalized `<h1>` and `<pre>` into its own
`span`/`font` markup, discarded the link `href`, stored quote as a plain
paragraph, and merged adjacent `<ul>`/`<ol>` into one bullet list. Follow-up
isolated probes proved H2 normalization and H3/link/quote/mixed-list loss. The
parser accepts only the observed H1/H2/code normalizations, and the editor
excludes Notes.app's create-time title line from the body. Exact IDs, HTML,
dates, timeout reconciliation, and GUI observations are recorded in
`tools/notes-probe/VERIFICATION.md`.

## Persistent cache

Notes.app remains authoritative. The local SQLite cache is derived, disposable,
and rebuildable: schema `user_version = 1` stores JSON payloads in `cache_meta`,
`snapshots`, and `full_notes`. WAL and `PRAGMA foreign_keys = ON` are enabled;
v1 has no relational foreign-key constraints. Startup may bootstrap cached data,
then performs foreground live refresh only. States are `Cached`,
`CachedBackendUnavailable`, and `Live`; there is no background sync.

Cached title search uses in-memory summaries. A selected full note may be read
by stable ID from the cache; a missing body is explicit, not an empty note.
Cached mode is read-only and attachment preview/export remains live-only.
Successful live `get_note` is read-through cached; cache-write failures are
warnings and never invalidate the live preview.

`SqliteNotesCache::open` is strict; `open_or_recover` quarantines only corrupt
SQLite or newer schemas, handling exactly the DB, `-wal`, and `-shm` paths as
`.corrupt-<pid>-<counter>` before recreating v1. `--cache-info` reports local
metadata; `--cache-clear` is idempotent and removes only those owned paths.
`last_successful_refresh` records successful snapshot persistence. Snapshots
are current-context only, not a global mirror, so unrelated full notes are
retained. Attachment binaries, NoteStore access, and background sync are not used.

Successful live note creation and normal updates update this derived cache only
after the backend mutation succeeds: backend create/update, runtime update,
full-Note upsert, then current-context snapshot persistence. A failed create
or update performs no cache writes; a failed update also preserves the dirty
editor state under normal editor semantics. The full-Note upsert and snapshot
write are independent best-effort operations: if either fails after a successful
save, the saved runtime note remains visible, `DataSourceState` remains `Live`,
a cache warning is shown, and the backend mutation is never rolled back. An
upsert failure does not prevent the snapshot attempt, and a later successful
snapshot does not clear that warning. Unsaved new-note and editor content is
never persisted; cached mode has no offline mutation queue.

Conflict overwrite uses the same post-success synchronization path as a normal
update: successful overwrite backend update, runtime update, full-Note upsert,
then authoritative current-context snapshot persistence. A backend overwrite
failure performs no cache writes and leaves the local editor state available.
If either derived-cache write fails after a successful overwrite, the overwrite
remains successful, runtime remains updated, `DataSourceState` remains `Live`,
and a cache warning is shown without backend rollback. A failed full-Note upsert
does not prevent the independent snapshot attempt.

Successful live moves synchronize only after the backend move succeeds and the
current runtime context has been refreshed. The returned moved full Note is an
authoritative payload, so it is upserted by stable NoteId; the resulting
current-context `App.notes` snapshot is then persisted. This is not a global
source/destination-folder mirror. A backend move failure writes nothing to the
cache. A later cache failure leaves the move and runtime state successful,
keeps `DataSourceState` `Live`, and shows a warning only. No extra backend read
is made solely to enrich the cache.

Successful delegated delete synchronizes the derived cache only after Notes.app
confirms the delegated delete: runtime current-context removal, exact full-Note
removal by stable NoteId, then resulting current-context snapshot persistence.
The exact row removal is safe because backend success authoritatively identifies
that single note; partial snapshot absence never causes global pruning. A failed
delete performs no cache writes. Failed remove/snapshot writes after successful
delete are independent warnings only: the note remains moved to Recently Deleted,
runtime remains post-delete, and `DataSourceState` remains `Live` with no backend
rollback. Cached/offline mode remains read-only.

Phase 8 cache scope is complete: live create, normal update, conflict overwrite,
move, and delegated delete all update derived cache only after backend success.
The retained limitations are intentional: snapshots are current-context only,
there is no global cache mirror or stale-pruning pass, attachment binaries are
not cached, and there is no background synchronization or offline write queue.

## Configuration

Configuration loads once at startup from
`~/Library/Application Support/apple-notes-tui/config.toml`. All preferences
are optional; the defaults preserve the existing UI:

```toml
refresh_interval_seconds = 120
auto_refresh = true
preview_wrap = true
show_attachment_metadata = true
```

`refresh_interval_seconds` defaults to 60 seconds and accepts 5–86400.
`auto_refresh = false` disables periodic refresh and due-state accumulation,
but manual `r` remains available. `preview_wrap` applies only to note previews;
`show_attachment_metadata` changes only optional attachment details, never
attachment identity or preview/export behavior.

CLI overrides are `--refresh-interval <seconds>`, `--auto-refresh` /
`--no-auto-refresh`, `--preview-wrap` / `--no-preview-wrap`, and
`--show-attachment-metadata` / `--hide-attachment-metadata`. CLI overrides
file values, which override defaults; contradictory boolean pairs fail
explicitly. Invalid file values fall back to defaults with a non-fatal warning.
`--config-info` reports all local effective values and sources without
contacting Notes.app or the cache. No credentials are stored and this pass
does not reload configuration or make safety confirmations configurable.

Configuration can also be edited locally without starting the TUI:

```bash
apple-notes-tui --config-set auto_refresh=false
apple-notes-tui --config-set refresh_interval_seconds=120
apple-notes-tui --config-unset preview_wrap
apple-notes-tui --config-reset
apple-notes-tui --config-info
```

The supported keys are exactly `refresh_interval_seconds`, `auto_refresh`,
`preview_wrap`, and `show_attachment_metadata`. Booleans accept only `true` or
`false`; interval bounds are unchanged. Writes are atomic: a temporary file in
the same Application Support directory is flushed and renamed into place.
Supported fields are normalized; unknown TOML fields are retained semantically,
while comments, spacing, and key order may change. A malformed file or a
supported field with an invalid type is never overwritten. Reset removes all
supported fields and removes the config file if no unknown fields remain.
Changes apply at the next startup; there is still no watcher or live reload.

The TUI also offers a local-only Settings popup with `,`. It displays the
effective value and source for Auto refresh, Refresh interval, Preview wrap,
and Attachment metadata. Use `j`/`k` to choose, Space to toggle booleans, Enter
to edit the interval, `u` to inherit/unset, `r` to revert one staged change,
`R` to reset all persisted supported values, `s` to save, and Esc to discard. It writes only changed
fields through the same atomic typed writer, preserves unknown fields, and
refuses a malformed config file. Settings changes apply next startup; active
CLI overrides still win in the current process. Reset preserves unknown fields
and removes a supported-only config file. The popup does not expose mutation
safety, Notes.app, backend, or cache operations.

## Session continuity

The app separately keeps the last successfully selected account and folder in
`~/Library/Application Support/apple-notes-tui/session.toml`. This disposable,
non-authoritative file contains only stable `account_id`, `folder_id`, optional
`note_id`, completed `search_query`, optional preview scroll, and the last
normal browsing pane (`navigation`, `notes`, or `preview`). It contains no note
content, search draft/history/results, editor draft or cursor, search-input,
help, settings, popup, or modal state. It is distinct from both `config.toml`
preferences and the derived SQLite cache.

At startup, saved IDs are accepted only when they exist in the already loaded
live or cached account/folder context. A missing account falls back to the
normal default context; a missing folder falls back within its saved account.
Malformed or missing session state is ignored without blocking startup. After
the restored folder's normal notes load, the optional stable `note_id` is used
only if that exact note is already in the current `App.notes`; missing, moved,
or deleted notes use the existing first-note fallback. Duplicate or renamed
titles do not affect restoration. Successful account/folder/note navigation
atomically replaces this small file; failure is warning-only and never rolls
back runtime selection. Session restoration adds no global Notes crawl or
cache mirror. A completed title search is restored only after current-context
notes load, then filters locally; its saved note is selected only if visible.
Zero matches remain an active empty search. Search input drafts are never saved.
The optional vertical `preview_scroll` is remembered only for the exact restored
note, resets for a fallback note, and is clamped to current preview content.
It stores neither terminal dimensions nor per-note history; current
`preview_wrap` remains a configuration concern.
The saved browsing pane is restored only after context, completed search,
selection, and preview scroll have been restored. Preview safely falls back to
Notes when no selected note remains. Pane changes reuse the same deduplicated
atomic session write; focus restoration itself performs no backend or cache
work, and there is no per-context focus history.

## Editor recovery drafts

One local recovery file, `~/Library/Application Support/apple-notes-tui/editor-draft.json`, may contain unsaved editor text and rich document structure. It is separate from `session.toml` and the derived cache. Only a dirty CREATE or EDIT draft is retained; title, editable rich body, stable IDs, and the edit conflict baseline are stored atomically. It is never applied to Notes.app automatically.

After normal startup context restoration, the TUI asks whether to restore or discard a valid local draft. Restore opens a dirty editor only after explicit confirmation and retains the original conflict baseline; discard removes only the local file. The physical recovery file remains until authoritative UI-thread success for CREATE, UPDATE, or an explicitly authorized conflict overwrite; failed saves, including failed overwrites, and conflict detection retain it. A dirty draft also retains the editor's logical field/target character cursor and best-effort vertical editor scroll; clean editor navigation never creates a draft. Missing or stale position metadata is clamped against reconstructed content using the editor's Unicode-safe character offsets. Terminal dimensions and selection ranges are not stored. Cleanup failures are warning-only and never roll back an authoritative save. Malformed, unsupported, or unavailable-target drafts are left intact with a warning and cannot be silently retargeted. Undo history, modal state, attachments, and search input are not stored. A crash after backend success but before local cleanup can leave a stale draft; recovery still requires an explicit user decision. In particular, a stale CREATE draft is never retried automatically.

`editor-draft.json` can contain unsaved user-authored text. Use `apple-notes-tui --draft-info` to inspect concise metadata only (never the body or raw JSON). Use `apple-notes-tui --draft-clear` to permanently delete that one local recovery file, including a malformed draft. Both commands are standalone local-only operations: they do not start the TUI, access Notes.app or the cache, or modify `session.toml` or `config.toml`.

## Periodic live refresh

While the TUI is running, its event loop checks a centralized 60-second
interval and starts one short-lived standard-library worker for the read-only
backend traversal. The UI polls its result without blocking; reconciliation,
search updates, status rendering, and derived-cache persistence stay on the UI
thread and reuse the same refresh application path as manual `r`.
This refreshes the currently loaded context, reconciles selection by stable
NoteId, recomputes active title search locally, and uses the existing derived
cache persistence path. There is no worker, concurrent backend call, global
mirror, or offline write queue.

Automatic refresh is deferred while an editor, help, search input, or any modal
workflow is open. If the interval becomes due while unsafe, one refresh runs as
soon as normal browsing resumes. Only one automatic refresh can be in flight;
additional elapsed intervals coalesce into one due retry. Live mutation actions
and a duplicate manual `r` are not launched while that read is in flight. The
TUI retains at most one in-memory foreground UI intent, with the latest intent
winning. After the worker becomes idle, that intent is revalidated against the
current selection and enters the normal foreground flow. It stores no NoteId,
folder destination, or mutation authorization: delete only opens a new
confirmation and create only opens an editor. A pending foreground intent takes
priority over another due automatic refresh. This is not a persistent/offline
mutation queue. Stale worker results are discarded by a
request generation and cannot overwrite newer UI context. Cached/backend-
unavailable state periodically retries the live backend and can recover to
`Live`; failures retain the existing runtime state. There is no Tokio runtime,
thread pool, offline write queue, or global mirror.

When a foreground backend action is requested during an automatic refresh, the
read-only refresh is cooperatively cancelled: its current `osascript` child is
terminated and reaped, the worker releases the backend, and the retained intent
then enters its normal synchronous UI flow. Cancellation is non-error: it does
not alter runtime state, the cache, or `DataSourceState`. Manual `r` uses the
same preemption rule and runs once after release; normal transport timeouts
remain in effect when no foreground action preempts the refresh.

Manual `r` uses that same non-blocking read worker rather than a synchronous
backend traversal. Manual refresh has priority over automatic refresh, resets
the interval when requested, and suppresses an immediate redundant automatic
retry after it finishes. A foreground action can in turn cancel either
read-only worker; create, edit, move, delete, and attachment operations remain
the existing synchronous foreground paths after backend ownership is released.

Startup live reconciliation is also non-blocking. The TUI restores the derived
cache, renders a usable first frame, and then schedules exactly one background
`accounts` -> `folders` -> `notes` read. Cached navigation remains usable while
the result is in flight; the UI thread applies the authoritative result and
persists the derived snapshot when it arrives. A startup read failure preserves
valid cached data and enters the existing backend-unavailable state. The
periodic timer does not launch a duplicate startup traversal.

Ordinary live browsing is cache-first as well. Folder activation presents
cached rows for the exact stable FolderId before scheduling the authoritative
Notes read. Selecting a note presents its cached full preview immediately and
refreshes it in the background. Only one navigation read is active; rapid
selection coalesces to the latest NoteId and stale folder/preview results are
discarded by stable IDs and generations. Notes.app remains authoritative, and
live results are reconciled and cached on the UI thread.

Visited folder summary rows are retained in memory by stable `FolderId` during
the session, so returning to a folder can reuse its cached presentation while
the authoritative validation read runs. Startup, periodic, and manual refresh
requests are distinguished in opt-in performance tracing; startup establishes
the next periodic deadline instead of triggering an immediate duplicate full
refresh.

CREATE and normal saves of existing notes use one short-lived worker: the editor is
temporarily frozen while it reads for conflict detection and performs the
ordinary update. The UI thread receives the result and calls `finish_saved`, so
runtime reconciliation and derived-cache writes remain unchanged. A failed or
conflicting save retains the dirty editor. After explicit overwrite
authorization, conflict overwrite also uses the worker; move and delete remain
only confirmed moves use the worker. The UI thread then reloads the current
source context without cache read-through, upserts the authoritative moved Note,
and persists the resulting snapshot. Delete remains synchronous.

Confirmed delegated delete now also uses the worker, but only after explicit
`y`/`Y`. The UI thread reloads the source context, removes exactly that stable
NoteId from the full-note cache, persists the resulting snapshot, and retains
the `Moved to Recently Deleted` status. It never globally prunes cached notes.

Attachment popup navigation remains local. Explicit preview and export now use
the same non-blocking worker, return their status on the UI thread, and never
write attachment payloads or metadata into the derived Notes cache. Cached and
offline modes continue to block those backend actions.

## Folder management

Press `N` while browsing an account or one of its folders to create a new
top-level folder in that selected account. The name popup accepts Unicode and
rejects empty or whitespace-only input. Creation uses the existing foreground
worker and does not speculatively insert a folder. Once Notes.app returns the
authoritative stable `FolderId`, the UI reloads the target account's folders,
selects that ID, clears current-folder search, and persists normal session and
derived-cache state. If
Notes.app rejects creation, the exact entered name stays in the
popup for editing or retry. Once Notes.app has created the folder, local session
or cache persistence warnings never roll it back or retry creation.
If session continuity cannot be saved after creation, the authoritative folder
remains created and selected; the warning is local only.

Press `R` on a selected folder to rename it. The rename is asynchronous and
targets the stable folder ID, never a display name. Existing note/search/preview
context remains in place after success; the derived current-context snapshot is
updated from authoritative runtime folder metadata. The bridge validates the
returned account and folder IDs, and the AppleScript resolves the folder only
within the requested account. A snapshot-write failure after a successful rename
is a local cache warning only: the authoritative renamed folder remains in the
runtime and is not retried or rolled back. System-folder renameability
is backend-authoritative because the current folder model has no typed system
marker. Nested-folder creation and same-account reparenting are available as
described below.

### Folder delete

With navigation focused and no note selected in the folder, press `D` to
confirm deletion of the selected folder;
only `y` or `Y` dispatches the backend operation. The backend foundation
targets only stable `AccountId` plus `FolderId`, resolves that ID only within the
requested account, and never transports a display name. Apple documents that
deleting a non-empty Notes folder moves its notes to Recently Deleted for 30
days; the documented outcome for child folders is not specific enough for this
client. Consequently this foundation conservatively permits deletion only of an
empty folder with no child folders. Notes.app remains authoritative for special
or system folders: no display-name heuristic is used. Backend deletion runs in
the existing foreground worker without speculative runtime removal. On success,
the runtime removes the exact stable ID, selects the next folder when available,
clears the old folder search, resets preview context, then persists session and
the derived snapshot. Cache/session failures are warning-only and never recreate
or retry the authoritative deletion. Normal tests use mocks and never delete a
real Notes folder.

### Nested-folder backend foundation

`Folder` already stores stable account/folder IDs, `FolderParent` (`Account` or
`Folder` with a stable parent ID), and `shared`; the recursive folder probe and
derived cache preserve that metadata. Navigation already renders the returned
hierarchy as a flattened depth-first tree. No nested-folder TUI, cache
lifecycle, or session schema has been added here.

The AppleScript dictionary exposes folders as children of both accounts and
folders, plus standard `make … at` and `move … to` commands. The backend
provides account-scoped `CreateChildFolder` and `ReparentFolder` contracts using
stable IDs only. Reparenting uses `Some(FolderId)` for a parent and `None` for
the account root; cross-account targets, self-parenting, and descendant cycles
are rejected by the mock and preflighted by AppleScript. Returned
account/source/parent identity is validated by the bridge.

Runtime behaviour for shared and special folders, cross-account moves, and
ordering remains manual-probe-only. Cross-account moves are intentionally
unsupported. The ignored child-create and reparent probes never run normally.

With navigation focused on a folder, `C` opens a separate Unicode-safe
subfolder popup naming that parent. It captures only the stable account and
parent-folder IDs, dispatches through the existing foreground worker on Enter,
and never inserts speculatively. Failure restores the exact input for retry.
Success selects the authoritative returned child ID after rebuilding the
depth-first tree, clears search, resets preview/note selection, then persists
the derived snapshot and session. Local cache/session failure is warning-only;
reparent UI remains unimplemented.

Child creation is non-blocking: while its worker owns the backend there is no
speculative hierarchy, cache, or session write. A child-create intent made
during a read refresh opens only this popup after the refresh releases; a fresh
explicit Enter is still required. A post-success session-write warning leaves
the authoritative child selected and is never retried or rolled back.

With navigation focused on a folder, `M` opens a same-account reparent popup.
It offers a typed account-root target plus eligible stable-ID folder targets;
the source folder and its descendants are excluded. Confirming the current
parent is a local no-op. A confirmed move uses the existing foreground worker,
does not change the tree speculatively, and on authoritative success rebuilds
the depth-first navigation while retaining the selected folder/note, search,
preview scroll, and focus. Snapshot persistence is warning-only after success;
session continuity IDs do not change, so no session rewrite is needed. There is
no drag/drop, cross-account reparent, rollback, or replay.

### Phase 15 A2.4c — deterministic refresh and retention closure

The periodic interval is measured from the refresh request, preventing an
immediate duplicate automatic traversal after startup or a manual refresh.
Manual refresh remains available before the periodic deadline. Loaded folder
summary rows are retained by stable `FolderId`, replaced only by an
authoritative live folder result, and invalidated by note/folder mutations.
Navigation workers apply only current-generation results, so stale folder or
preview reads cannot overwrite newer runtime state. The current-context
refresh sequence remains accounts -> folders -> notes -> selected preview ->
attachments; cache writes remain on the UI thread. No global mirror,
NoteStore access, or transport/process optimization was added.
