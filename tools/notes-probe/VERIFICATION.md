# Phase 0 local verification record

## Phase 7 TUI runtime search

Phase 7 search/filtering is implemented only in `notes-tui` runtime state.
`App.notes` remains the authoritative loaded `NoteSummary` collection and the
typed search state holds only a query plus visible stable `NoteId` references.
No NoteStore.sqlite access, persistent index, background synchronization, or
real Notes.app write operation is used.

Automated coverage verifies case-insensitive English/Russian/German/emoji
title matching, Unicode-safe editing and clearing, zero-result safety, modal
and editor isolation, filtered delegated-delete success/failure, reload
recomputation, folder-transition clearing, and backend call-count guarantees.
Title-only matching performs no `get_note` calls while typing or applying a
query. Demo data includes Alpha, Russian, German, emoji, and body-token
examples for manual filtering checks; bodies are intentionally not searched in
this phase.

Manual/read-only validation is limited to the demo backend and the existing
read-only probe script. No new Notes.app fixture or mutation is required.

Date: 2026-08-26

Environment: macOS, Codex desktop workspace

## Completed

- Cargo workspace check passed.
- Five Rust unit tests passed, covering JSON escaping, mutation guards, and
  runtime script resolution relative to the executable.
- The AppleScript compiled successfully against the installed Notes.app
  scripting dictionary when compilation was allowed outside the workspace
  sandbox.
- create-note, update-note, move-note, and delete-note were exercised in
  dry-run mode. Each returned a valid apple-notes-probe/v1 JSON envelope.
- Example fixture JSON and the read-only verification shell script passed
  offline syntax/format validation.

## Relocated runtime result

The runtime binary was rebuilt and installed at tools/notes-probe/notes-probe.
It successfully resolved scripts/notes_probe.applescript from that executable
directory after the repository had moved to its permanent location.

Read-only accounts, folders, notes, and snapshot probes succeeded with Notes
Automation permission. notes --limit 5 returned five real notes without a
-1728 error. The Unicode folder Семья also enumerated successfully.

snapshot now uses a single accounts-to-folders-to-notes traversal with bulk
metadata and attachment-group queries per folder. It does not read note HTML or
plaintext. On the real 127-note library, the default 50-item snapshot completed
in 4.753 seconds; a full limit-0 snapshot returned 127 unique IDs with no
duplicates. The 30-second runner timeout was unchanged.

~~~text
snapshot: ok=true, notes.returned=50, notes.total=127, notes.truncated=true
~~~

## Controlled Write Lifecycle

The real lifecycle was run only against one uniquely named test note in these
two folders:

- `Apple Notes TUI Test` (source)
- `Apple Notes TUI Test Moved` (destination)

Both folders were created through Notes.app scripting only when absent. No
other user notes were created, updated, moved, or deleted. Before every actual
note mutation, the corresponding probe was run without `--execute`; each
returned a successful `dryRun: true` envelope. The actual operations used both
`--execute` and `--write-ack I_UNDERSTAND_NOTES_WILL_CHANGE`; deletion also
used an exactly matching `--confirm-note-id`.

1. `create-note` created `apple-notes-tui Phase0 F0375ED8-994A-4850-ACBC-4FB14DB36EE0`
   with `fixtures/write-lifecycle-initial.html`.
2. `get-note` returned its ID, name, account ID, source folder ID, HTML,
   plaintext, dates, and protection/shared flags. The HTML retained paragraph
   structure, bold and italic formatting (normalized by Notes from `strong` /
   `em` to `b` / `i`), an unordered list, ASCII, Russian text, German umlauts,
   and emoji.
3. `update-note` changed both name and body using
   `fixtures/write-lifecycle-updated.html`. Its modification date changed from
   `Wednesday, 26. August 2026 at 12:54:38` to
   `Wednesday, 26. August 2026 at 12:55:09`; a read-back confirmed the updated
   semantic HTML, plaintext, Unicode text, and emoji.
4. `move-note` put the same note in `Apple Notes TUI Test Moved`. A folder
   query then returned zero notes in the source folder and exactly that note in
   the destination folder; `get-note.folderId` matched the destination.
5. `delete-note` removed it from the destination folder. Notes.app moved it to
   `Recently Deleted`, so `get-note` correctly returned the same ID there and
   the former destination folder was empty. This is Notes.app's recoverable
   deletion behavior, rather than a permanent-not-found result.

The two empty named test folders and the single recoverable test note in
`Recently Deleted` remain as visible audit artifacts. No direct SQLite access
was used.

## Phase 4 real Notes verification

Date: 2026-08-27

The real TUI was used only in the initially empty `Apple Notes TUI Test`
folder. It created these controlled fixtures, which remain in that folder:

- `Phase4-Rich-20260827-170059` —
  `x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p479`
- `Phase4-List-20260827-170059` —
  `x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p480`

Initial `get-note.bodyHtml` values were:

~~~html
<div>Phase4-Rich-20260827-170059</div>
<div><b><span style="font-size: 24px">Phase 4 Rich Heading</span></b></div>
<div><b>Bold paragraph via TUI</b><br></div>
<div><i>Italic paragraph via TUI</i><br></div>
<div><u>Underline paragraph via TUI</u><br></div>
<div><u>Example link via TUI</u><br></div>
<div>Русский текст: Привет, мир</div>
<div>Deutsch: Grüße Größe über</div>
<div>Emoji: 🚀✨</div>
<div>Quote preserved by Notes</div>
<div><font face="Courier"><tt>Code sample: let x = 42;</tt></font></div>
~~~

~~~html
<div>Phase4-List-20260827-170059</div>
<ul>
<li>Bullet alpha via TUI</li>
<li>Bullet beta via TUI</li>
<li>Numbered one via TUI</li>
<li>Numbered two via TUI</li>
</ul>
~~~

The exact returned HTML was exercised through `parse_notes_html` in a
regression test. It recovered heading, bold, italic, underline, Russian,
German, emoji, and code semantics. Notes.app had already removed the link
`href`, reduced quote to a plain paragraph, converted `<h1>` to a bold 24 px
`span`, converted `<pre>` to Courier `font`/`tt`, and merged the adjacent
bullet and numbered lists into one `<ul>`. These lossy changes were therefore
not reconstructed by guessing. Parser support was added only for the observed
heading/code forms; the create-time body line that duplicates the note name is
excluded from the editor.

The TUI then reopened only these two fixtures. It prefixed the Russian rich
paragraph with `UPDATED ` and the first bullet item with `UPDATED `. Read-back
changed modification dates from `Thursday, 27. August 2026 at 17:01:55` to
`Thursday, 27. August 2026 at 17:09:04` for the rich note, and from `Thursday,
27. August 2026 at 17:02:37` to `Thursday, 27. August 2026 at 17:10:42` for the
list note. A before/after semantic regression test confirmed every unrelated
block, item, style, and position remained equal. Final HTML was:

~~~html
<div><b><span style="font-size: 24px">Phase 4 Rich Heading</span></b></div>
<div><b>Bold paragraph via TUI</b><br></div>
<div><i>Italic paragraph via TUI</i><br></div>
<div><u>Underline paragraph via TUI</u><br></div>
<div><u>Example link via TUI</u><br></div>
<div>UPDATED Русский текст: Привет, мир</div>
<div>Deutsch: Grüße Größe über</div>
<div>Emoji: 🚀✨</div>
<div>Quote preserved by Notes</div>
<div><font face="Courier"><tt>Code sample: let x = 42;</tt></font></div>
~~~

~~~html
<ul>
<li>UPDATED Bullet alpha via TUI</li>
<li>Bullet beta via TUI</li>
<li>Numbered one via TUI</li>
<li>Numbered two via TUI</li>
</ul>
~~~

Notes.app GUI inspection matched the probes. The rich note visibly showed the
heading, bold, italic, underlines, Unicode and emoji, plus a code block; its
link label was underlined but not exposed as a link and quote had paragraph
appearance. The list note visibly showed all four items as bullets. No other
user notes were created, opened for editing, moved, or mutated.

## Phase 4.1 controlled transport probes and reconciliation

Date: 2026-08-27

No Phase 4 fixture (`p479`/`p480`) was modified. Phase 4.1 used only isolated
probe notes. After the mixed-list runner timeout, all reconciliation was
read-only; no probe was updated, moved, or deleted. The fixtures remain audit
artifacts and must not be cleaned up automatically.

### H2 — supported with Notes normalization

- Title: `Phase41-Probe-H2-20260827`
- ID: `x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p481`
- Input body: `<h2>Probe H2</h2>`
- Exact read-back:

~~~html
<div>Phase41-Probe-H2-20260827</div>
<div><b><span style="font-size: 18px">Probe H2</span></b></div>
~~~

The GUI retained the H2 appearance. The strict parser recognizes only this
observed bold 18 px span form as H2. During reconciliation, `get-note` reported
`folderId` `.../ICFolder/p3` (`Recently Deleted`) for p481 even though it had
been created in `Apple Notes TUI Test`. The note was not modified to investigate
or correct that anomaly.

### H3 — unsupported-lossy

- Title: `Phase41-Probe-H3-20260827`
- ID suffix: `/ICNote/p482`
- Input body: `<h3>Probe H3</h3>`
- Exact read-back:

~~~html
<div>Phase41-Probe-H3-20260827</div>
<div><b>Probe H3</b></div>
~~~

The result is ordinary bold text; no recoverable H3 marker remains. Literal
`<h3>` remains supported by the strict parser for mock/demo documents, but the
Apple Events backend advertises H3 as unsupported. Plain `<div><b>…</b></div>`
is never inferred to be H3.

### Hyperlink — unsupported-lossy

- Title: `Phase41-Probe-Link-20260827`
- ID suffix: `/ICNote/p483`
- Exact read-back:

~~~html
<div>Phase41-Probe-Link-20260827</div>
<div><u><font color="#0000EE">Example Link</font></u></div>
~~~

The original `href` is absent. Underline and blue color are visual residue,
not enough evidence to reconstruct the link target.

### Quote — unsupported-lossy

- Title: `Phase41-Probe-Quote-20260827`
- ID suffix: `/ICNote/p484`
- Input body: `<blockquote>Probe Quote</blockquote>`
- Exact read-back:

~~~html
<div>Phase41-Probe-Quote-20260827</div>
<div>Probe Quote</div>
~~~

No quote marker remains in either the HTML or GUI appearance.

### Directly adjacent mixed list types — unsupported-lossy

The attempted mixed-list create returned after 30 seconds with indeterminate
state:

~~~json
{
  "schemaVersion": "apple-notes-probe/v1",
  "operation": "create-note",
  "ok": false,
  "error": {
    "source": "runner",
    "code": "osascript_timeout",
    "message": "osascript timed out after 30 seconds; Notes.app state may be indeterminate"
  }
}
~~~

Read-only reconciliation in `Apple Notes TUI Test` found two exact-title
artifacts. p485 is the earlier artifact; p486 is the later duplicate produced
when the timed-out operation completed after the runner had returned. Neither
was deleted or modified.

- Title (both): `Phase41-Probe-MixedList-20260827`
- ID suffixes: `/ICNote/p485` and `/ICNote/p486`
- Exact read-back from both notes:

~~~html
<div>Phase41-Probe-MixedList-20260827</div>
<ul>
<li>Bullet one</li>
<li>Bullet two</li>
<li>Numbered one</li>
<li>Numbered two</li>
</ul>
~~~

The adjacent input UL and OL boundary was lost and all four items became one
unordered list. Bullet and numbered lists are still supported individually;
only direct UL-to-OL or OL-to-UL adjacency requires the unsupported
`mixed_adjacent_list_types` capability. A paragraph between the lists does not
require it.

### Evidence-backed Apple Events capability result

H1, H2, bold, italic, underline, code, bullet list, and numbered list are true.
H3, hyperlink, quote, and directly adjacent mixed list types are false. H1,
H2, and code are supported with the exact Notes.app normalization described
above and in the Phase 4 record.

## Phase 5 read-only attachment investigation

Date: 2026-08-28

No note, body, folder, or attachment was created, updated, moved, renamed, or
deleted. Fixtures p479-p486 were not opened for mutation. Investigation used a
full metadata snapshot followed only by `attachments` queries for the six
existing notes whose `attachmentCount` was nonzero. The discovery snapshot was
complete and unique:

~~~text
operation=snapshot ok=true returned=138 total=138 truncated=false duplicates=0
~~~

### Scripting dictionary evidence

The installed Notes.sdef declares these readable attachment properties:

- `name` (text)
- `id` (text, unique identifier)
- `container` (parent note)
- `content identifier` (content-id URL used in note HTML)
- `creation date`
- `modification date`
- `URL` (only "for URL attachments, the URL the attachment represents")
- `shared`

It declares the class as `attachment`, but exposes no UTI, MIME type, byte
size, or file path. A hidden `contents` file property is explicitly documented
as private so Notes does not reveal its internal storage location. The class
officially responds to `show` and Cocoa Standard `save`; Notes declares only
its native `public.item` save format. This is the evidence for Notes-native
preview and export-copy support, not evidence for a local path or direct Quick
Look support.

### Exact real metadata

All IDs below share the exact Core Data prefix
`x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/`. `URL` was `null` and
`shared` was `false` for every real attachment.

| Attachment ID suffix | Note ID suffix | Exact name | Exact content identifier | Creation / modification date |
| --- | --- | --- | --- | --- |
| `ICAttachment/p393` | `ICNote/p392` | `Hetzner-VPN.pdf` | `cid:E69B68AD-1476-4A6A-9799-C3051E7BAF57@icloud.apple.com` | `Wednesday, 25. February 2026 at 09:54:56` / same |
| `ICAttachment/p210` | `ICNote/p209` | `fb9f7922c4a40b2829469f5280296c02.jpeg` | `cid:FFDAF9A3-5429-4400-88C8-525A64424AC7@icloud.apple.com` | `Tuesday, 12. August 2025 at 12:24:28` / same |
| `ICAttachment/p211` | `ICNote/p209` | `cf6ee1ea81d2187a4d067f597ce6801c.png` | `cid:512E971C-3DB6-4DA9-8BDA-F35836066BAD@icloud.apple.com` | `Tuesday, 12. August 2025 at 12:24:28` / same |
| `ICAttachment/p212` | `ICNote/p209` | `Attachment.png` | `cid:2FFE7526-0C59-4421-BF56-02DD579C05D2@icloud.apple.com` | `Tuesday, 12. August 2025 at 12:24:28` / same |
| `ICAttachment/p168` | `ICNote/p154` | `SHVEDCHENKO.tif` | `cid:1A29FA82-DAC1-4F4D-B6E3-A790688B492B@icloud.apple.com` | `Friday, 14. February 2025 at 10:04:34` / same |
| `ICAttachment/p160` | `ICNote/p154` | `Screenshot 2025-02-13 at 16.25.59.png` | `cid:8750F28B-C9F8-4D3D-B667-FA187173E857@icloud.apple.com` | `Friday, 14. February 2025 at 10:01:08` / same |
| `ICAttachment/p164` | `ICNote/p154` | `Screenshot 2025-02-14 at 09.57.52.png` | `cid:FF620346-CD89-46DC-A310-CE47BA19DCEC@icloud.apple.com` | `Friday, 14. February 2025 at 10:01:59` / same |
| `ICAttachment/p124` | `ICNote/p125` | `Изображение.heic` | `cid:DB76236A-F2AE-49D4-BA89-F5E91352C44E@icloud.apple.com` | `Tuesday, 5. March 2024 at 19:39:33` / same |
| `ICAttachment/p85` | `ICNote/p84` | `Screenshot 2023-01-05 at 16.21.25.png` | `cid:C32D51BD-550F-428C-A6CC-799D924AC754@icloud.apple.com` | `Thursday, 5. January 2023 at 16:21:09` / `Thursday, 5. January 2023 at 16:21:29` |
| `ICAttachment/p88` | `ICNote/p82` | `0.00121504.jpeg` | `cid:15A6EDD4-5A80-40CF-9136-B997E7C4C666@icloud.apple.com` | `Sunday, 11. December 2022 at 15:47:02` / `Wednesday, 4. January 2023 at 10:11:30` |

The exact PDF response used for the bridge regression fixture was:

~~~json
{
  "schemaVersion": "apple-notes-probe/v1",
  "operation": "attachments",
  "ok": true,
  "data": [{
    "id": "x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICAttachment/p393",
    "noteId": "x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p392",
    "name": "Hetzner-VPN.pdf",
    "contentIdentifier": "cid:E69B68AD-1476-4A6A-9799-C3051E7BAF57@icloud.apple.com",
    "url": null,
    "creationDateText": "Wednesday, 25. February 2026 at 09:54:56",
    "modificationDateText": "Wednesday, 25. February 2026 at 09:54:56",
    "shared": false
  }]
}
~~~

### Capability result and live smoke

| Capability | Result |
| --- | --- |
| Metadata list | Supported with the exact fields above |
| UTI / MIME / size | Unavailable; not in Notes.sdef |
| Local file URL/path | Unavailable for the real file attachments; `URL` was always null |
| Preview/open | Supported for known file kinds through explicit `show attachment` |
| Quick Look by path | Unavailable because no supported local path exists |
| Export copy | Supported through `save attachment in file`; destination is chosen in the macOS Save dialog |
| URL attachment open | Disabled; URL attachments remain metadata-only |
| Unknown-kind preview | Disabled with typed `UnsupportedKind` status; export copy remains available |
| Locked/protected note | Attachment list, preview, and export blocked with typed `ProtectedNote` status |
| Body editing | Existing attachment notes remain read-only; lossless-save policy is unchanged |

One explicit read-only preview smoke used existing PDF attachment p393. It
returned exactly:

~~~json
{
  "schemaVersion": "apple-notes-probe/v1",
  "operation": "preview-attachment",
  "ok": true,
  "data": {
    "id": "x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICAttachment/p393",
    "noteId": "x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p392",
    "method": "Notes.app show"
  }
}
~~~

The preview selected/showed the existing object in Notes.app. It did not
create an export file or mutate Notes. `show attachment` left a modal preview
open in Notes; snapshot calls timed out until that preview was closed with Esc,
then returned normally again. Users must close the Notes preview before
returning to the TUI. The ignored bridge integration test can repeat the
metadata/read-only preview with explicit existing IDs:

~~~sh
APPLE_NOTES_TUI_LIVE_ATTACHMENT_NOTE_ID='NOTE_ID' \
APPLE_NOTES_TUI_LIVE_ATTACHMENT_ID='ATTACHMENT_ID' \
cargo test -p notes-bridge tests::live_attachment_metadata_and_notes_preview_are_read_only \
  -- --ignored --exact
~~~

### Phase 5 final validation

- `cargo fmt --all --check`: passed.
- `cargo check --workspace`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  passed with no warnings.
- `cargo test --workspace`: 72 passed, 0 failed, 2 ignored manual live tests.
- Canonical `notes_probe.applescript`: `osacompile` passed against Notes.app.
- `tools/notes-probe/scripts/verify-readonly.sh`: accounts, folders, notes,
  and snapshot all returned valid JSON.
- Demo TUI: popup open/close, j/k navigation, supported PDF preview,
  unsupported-kind error, and mock export-copy status were manually verified.

After the modal-preview observation and read-only cache hydration described
above, the required final full snapshot returned exactly:

~~~text
snapshot --limit 0: ok=true returned=139 total=139 truncated=false
itemsLength=139 uniqueIds=139 duplicates=[]
wallTimeSeconds=22.08536225
~~~

The library total changed from 138 during the initial Phase 5 discovery to 139
before final validation due to external Notes activity. Phase 5 performed no
note or attachment mutation and did not investigate or alter that external
change.

### Phase 6 delegated delete lifecycle

One dedicated controlled fixture was created in `Apple Notes TUI Test` and
then deleted through the guarded Notes.app AppleScript path; no permanent
deletion, restoration, or empty-trash operation was performed.

- Title: `Phase6-Delete-20260828`
- ID before and after: `x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p489`
- Source folder: `x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICFolder/p475`
- Recently Deleted folder: `x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICFolder/p3`
- Created and modified: `Friday, 28. August 2026 at 11:20:13`

Pre-delete HTML:

~~~html
<div>Phase6-Delete-20260828</div>
<div>Phase 6 controlled delete fixture</div>
~~~

Delete response:

~~~json
{"schemaVersion":"apple-notes-probe/v1","operation":"delete-note","ok":true,"data":{"id":"x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICNote/p489","name":"Phase6-Delete-20260828","sourceFolderId":"x-coredata://FB0B8283-2371-44B7-BC2B-58CC3ABAB4D4/ICFolder/p475","deleted":true,"disposition":"Notes.app controls recovery and permanent deletion behavior"}}
~~~

Read-only reconciliation found p489 absent from its source folder and present
in Recently Deleted with the same ID, title, dates, and body. `get-note` using
the original ID succeeds and returns folder p3. This confirms delegated trash
behavior (`DeleteDisposition::DelegatedToNotesApp`), not permanent deletion.

During this reconciliation p478–p481 were already present in Recently Deleted;
they were not modified or investigated during Phase 6.

## Phase 8 cache validation

Phase 8 uses strict scripted backend queues and injected filesystem-free cache
tests: bootstrap is once, cached backend failures retain data, retry returns to
Live, cache warnings do not replace live state, selection is stable by NoteId,
and cached search makes no backend calls. Cached full-note preview uses stable
IDs, distinguishes missing/read-error content, remains read-only, and live
`get_note` read-through upserts only successful results.

SQLite v1 persistence covers close/reopen snapshots and Unicode, full Notes and
attachment metadata, exact IDs, refresh metadata invariance on reads,
conservative partial-context retention, and transactional rollback after the
snapshot row write but before metadata/commit. Corrupt and newer-schema caches
are quarantined as exact DB/-wal/-shm owned paths with `.corrupt-<pid>-<counter>`;
siblings are preserved. Clear is idempotent; info is local-only with exact
counts. Pragmas are WAL and foreign_keys=1; v1 has no relational FK constraints.

No real Notes.app mutations, direct NoteStore access, attachment binaries,
global cache redesign, or background sync were performed in Phase 8 validation.

### Phase 8 mutation-to-cache — create

The create path is covered with the mock backend and filesystem-free FakeCache.
A successful create calls the backend once, upserts the returned full Note once,
replaces the current-context snapshot once, and persists the created stable
NoteId. A backend create failure still calls the backend once but performs zero
cache writes, keeping unsaved editor data out of the cache.

Independent post-success cache failures are warning-only: an upsert failure
still attempts snapshot replacement and its warning remains visible after that
snapshot succeeds; a snapshot failure after a successful upsert also keeps the
created runtime state and `DataSourceState::Live`. Neither case retries or rolls
back the backend create. No real Notes.app mutations were performed.

### Phase 8 mutation-to-cache — normal update

The normal edit/save path is covered with the same mock backend and FakeCache.
A successful update calls the backend once, preserves the stable NoteId and
selection, upserts the updated full Note once, and replaces the authoritative
current-context snapshot once. A failed update performs zero cache writes while
leaving dirty editor content available. Dirty editor changes before backend
success also perform zero cache writes.

For successful updates, an injected full-Note upsert failure still attempts the
snapshot write and retains its cache warning; an injected snapshot failure after
a successful upsert is likewise warning-only. Both cases keep the updated
runtime note and `DataSourceState::Live`, with no rollback or duplicate backend
update.

### Phase 8 mutation-to-cache — conflict overwrite

The real conflict path is covered: the normal save detects a changed remote
modification date, opens the conflict popup, and `o` invokes the overwrite path.
Successful overwrite calls the backend once, preserves the stable NoteId and
selection, then uses the shared `finish_saved` full-Note upsert and snapshot
replacement once each. The overwritten payload is verified in both runtime and
cache snapshot.

A backend overwrite failure performs zero cache writes while retaining the local
dirty editor state. Post-success upsert and snapshot failures are warning-only:
the independent snapshot still follows a failed upsert, the warning remains
visible, and runtime stays overwritten with `DataSourceState::Live`; there is no
rollback or duplicate update. No real Notes.app mutations were performed.

### Phase 8 mutation-to-cache — move

The real move popup path is covered: open the popup, select a destination, and
confirm. A successful backend move occurs once; its returned authoritative full
Note is upserted once with the destination-folder metadata, then the resulting
current-context `App.notes` snapshot is replaced once. The moved note disappears
from the source context and selection remains valid. This remains a
current-context cache, not a global source/destination-folder mirror.

A backend move failure performs zero cache writes and preserves the pre-move
runtime/selection. An injected snapshot failure after successful move is
warning-only: runtime remains post-move and `DataSourceState::Live`, with no
rollback or duplicate move. No extra backend reads are added solely for cache
synchronization. No real Notes.app mutations were performed.

### Phase 8 mutation-to-cache — delegated delete

The real TUI `D` → confirmation → `y` delegated-delete path is covered. A
successful backend delete occurs once, preserves the existing safe selection
policy, removes exactly the deleted stable NoteId from the runtime source
context, removes exactly that full-Note cache row once, and persists the
resulting current-context snapshot once. The UI remains `Moved to Recently
Deleted`.

`CacheStore::remove_note` is exact-ID and idempotent: SQLite deletes only the
matching `full_notes` row, leaves snapshots and refresh metadata untouched, and
never infers deletion from a partial snapshot. Tests verify unrelated full Notes
remain present. A backend delete failure performs zero cache writes and leaves
runtime/selection unchanged. Injected remove and snapshot failures remain
warning-only after backend success: the snapshot still follows a failed remove,
the warning remains visible, `DataSourceState::Live` remains, and there is no
rollback or duplicate delete. No real Notes.app mutations were executed.

Phase 8 is complete for the derived-cache scope: every live mutation path is
post-success synchronized and tested for backend-failure no-write and
cache-failure warning-only behavior. The cache remains current-context only;
there is no global mirror, global pruning, attachment-binary cache, background
synchronization, or offline mutation queue.

## Configuration

Phase 10 A2 adds independently optional `auto_refresh`, `preview_wrap`, and
`show_attachment_metadata` preferences, all defaulting to `true` to preserve
existing behavior. File values, defaults, and explicit boolean CLI overrides
are resolved per preference; contradictory CLI pairs are rejected and malformed
file values retain the non-fatal fallback warning. `--config-info` reports each
effective value and source without backend or cache access.

Deterministic TUI coverage proves `auto_refresh = false` starts no periodic
backend traversal and accumulates no deferred due state, while manual refresh
still works. Preview wrapping can be disabled without changing note content,
including Unicode text and cached previews. Attachment metadata can be hidden
without hiding attachment identity or changing preview/export request identity.
Configuration remains startup-only: no watcher, live reload, settings UI,
or configurable mutation safety was added; no real Notes.app mutation was
performed.

## Phase 10 B1 — local config editing

The standalone `--config-set key=value`, `--config-unset key`, and
`--config-reset` commands operate only on the Application Support config path
and exit before backend, cache, or TUI initialization. They accept only the
four typed supported fields; booleans are strict `true`/`false` and refresh
intervals retain their 5–86400 validation. Direct edits reject unknown keys,
invalid values, mixed runtime/write invocations, malformed files, and existing
supported fields with invalid types.

Focused temporary-directory tests prove parent creation for explicit writes,
single-field update/unset behavior, preservation of other supported and unknown
fields, reset removal of a supported-only file, preservation of unknown fields
on reset, and malformed-file non-overwrite. The writer creates a same-directory
temporary file, flushes it with `sync_all`, then renames it atomically; a write
failure before rename leaves the original target unchanged. Formatting/comments
may normalize, but unknown fields remain semantically intact. No real Notes.app
mutation, live reload, or config watcher is involved.

## Phase 10 B2 — Settings popup

The `,` shortcut opens a modal local Settings popup only from stable browsing
state. It shows the four existing safe preferences with their effective values
and default/file/CLI sources. `j`/`k` select, Space toggles booleans, Enter
edits the bounded numeric interval, `s` saves, and Esc discards the draft.
Draft changes affect neither runtime nor disk before Save. Save uses the B1
typed atomic writer against the current on-disk file, writes only dirty fields,
preserves unknown values, and refuses malformed data without closing the popup.
The runtime remains unchanged; successful status explicitly says changes apply
at next startup and notes active CLI precedence. Tests use temporary paths and
prove no backend/cache/refresh/mutation worker activity. There is no reset or
unset UI, live reload, safety toggle, or real Notes.app mutation.

## Phase 11 A1 — session continuity

The TUI now keeps a separate disposable `session.toml` containing only the
stable selected `account_id` and `folder_id`. Temporary-path tests cover a
missing-file no-op, ID round-trip, malformed-state ignore/fallback, duplicate
account/folder display names restored by ID, missing-account fallback, and
missing-folder fallback inside the retained account. The saved context is
validated only against account/folder data already loaded by normal live or
cache-first startup; no global backend traversal, cache mirror, note content,
search state, editor state, or settings state is involved.

Successful committed folder navigation writes one coherent account/folder pair
through a same-directory temporary file, `sync_all`, and atomic rename. A
session-write failure is warning-only: runtime navigation and `DataSourceState`
are not rolled back. Backend/navigation failures do not persist new state.
Cached/backend-unavailable startup validates the saved IDs against the cached
context and safely falls back when absent. No real Notes.app mutation was used.

## Phase 11 A2 — selected-note continuity

`session.toml` now optionally records `note_id` alongside the existing stable
account/folder pair. Legacy A1 files without it load as `None`. Once the normal
current-context note list is loaded, the app restores that exact stable ID only
when it is present in `App.notes`; duplicate titles and renamed titles are not
used as identity. Missing, moved, or deleted notes retain the existing
first-visible-note fallback without a global lookup, extra `get_note`, or
folder traversal. Cached and backend-unavailable startup use the same
already-loaded cached note list.

Committed note selection, search-visible selection, and post-create/move/delete
selection normalization persist one coherent account/folder/note triplet. The
search query itself is never saved. Update or failed mutation paths do not
write speculative selection state. The A1 atomic writer remains in use;
write failures leave runtime and `DataSourceState` intact and surface only a
session warning. Tests use temporary paths and no real Notes.app mutation.

## Phase 11 A3 — completed search continuity

Session state optionally retains only a completed UTF-8 title `search_query`.
It is applied locally after current-context notes load, recomputes visible IDs
with the existing matcher, and selects the saved `NoteId` only when visible.
Legacy files remain compatible; zero-match queries remain active and drafts,
history, results, and note content are not stored. Cached/backend-unavailable
startup needs no backend read for this local restore. Search apply/clear writes
the same atomic session file; write failures are warning-only.

## Phase 11 A4 — preview-scroll continuity

The optional `preview_scroll` session field is a `u16` vertical Paragraph row
offset. It is restored only when the exact saved `NoteId` remains selected
after current-context search reconciliation; fallback and empty-search states
start at zero. Restore uses already loaded preview data, clamps to the current
logical preview line range, and performs no backend/cache read. Scroll changes
reuse the atomic session writer and are deduplicated with the coherent session
snapshot; failed writes are warning-only. No terminal dimensions, per-note
history, editor, focus, or modal state is persisted.

## Phase 11 A5 — browsing-focus continuity

Session state now optionally records one safe browsing focus value:
`navigation`, `notes`, or `preview`. Legacy A1–A4 files without it load as
`None`; unknown values make the small session file malformed and retain the
normal startup fallback. Restore occurs after context, completed search, stable
note selection, and preview-scroll reconciliation. Preview falls back to Notes
when no selected current-context note exists, including a restored zero-match
search. Tab, Shift-Tab, Left/Right, and h/l focus changes persist the coherent
session snapshot only when the browsing pane actually changes. Help, Settings,
search editing, editor state, and popups never replace that value. Tests use
temporary session paths, prove the fallback and deduplication behavior, and
perform no backend/cache operation or real Notes.app mutation solely for focus
continuity. Session-write failures remain warning-only and never roll back
runtime focus or `DataSourceState`.

## Phase 11 B1 — editor recovery draft

The TUI now keeps at most one versioned `editor-draft.json` outside session and
cache state. It serializes the actual editable `RichDocument`, title, CREATE or
EDIT identity, and EDIT conflict baseline; Unicode and multiline content round
trip exactly. Dirty editor mutations use the existing atomic local-file writer
and equality deduplication. Startup discovers the file without backend/cache
work and presents an explicit Restore/Discard popup: it never silently opens a
dirty editor or mutates Notes.app. Restore preserves the saved edit baseline;
discard and explicit editor cancel remove only the recovery file. Successful
saves clean it up after UI-thread reconciliation; failed saves and conflicts
retain it. Malformed or unsupported files are warned about and left untouched.
Focused regressions also cover injected write and cleanup failures: editor
runtime state is never rolled back, old bytes survive a failed replacement, and
cleanup warnings do not alter `DataSourceState`. Channel-gated CREATE, normal
UPDATE, and explicit conflict-overwrite workers prove that the physical draft
file remains readable with its exact content and original EDIT baseline until
the UI receives authoritative success; only then does `finish_saved()` attempt
cleanup. A failed explicit overwrite leaves the dirty editor and exact draft in
place, performs no cleanup or cache write, and does not retry. Missing edit
targets and create destinations remain local orphan drafts and cannot be
restored into an unrelated note or folder. These checks use explicit channels,
not correctness sleeps, and perform no real Notes.app mutation.

Phase 11 B2 extends that one dirty recovery file with a typed logical editor
cursor (field, document target, Unicode-safe character offset) and a best-effort
vertical editor scroll. Legacy B1 drafts omit those optional fields and restore
with normal defaults. Exact dirty position changes rewrite the existing draft;
identical snapshots are deduplicated, while clean cursor/scroll navigation never
creates one. Restore clamps stale targets, character offsets, and scroll values
against reconstructed content without losing valid draft text. Tests cover
Unicode, failed-update and conflict retention of the latest metadata, and prove
that position recovery has no backend or cache writes. Terminal geometry,
selection ranges, undo/redo, modal state, and multi-draft history remain out of
scope.

Phase 11 B3 adds standalone local draft management. `--draft-info` reports
valid CREATE/EDIT metadata without body content; tests include a secret body
marker and prove it never reaches output. Missing drafts succeed, while malformed
and unsupported drafts are inspected without rewriting or deleting them.
`--draft-clear` directly removes only `editor-draft.json`, remains idempotent,
and can explicitly clear malformed data without parsing it. Temporary-path tests
prove session, config, and cache siblings remain untouched; parser tests reject
mixed runtime modes. These commands construct no backend, open no cache, and
perform no Notes.app operation.

## Phase 9 — periodic live refresh foundation

The TUI event loop uses a deterministic 60-second tick that starts a single
short-lived standard-library read worker and non-blockingly polls its result on
the UI thread. Tests cover interval/not-yet-due
behavior, one attempt per interval without storms, deferred refresh during dirty
editor and mutation popup workflows, and execution once safe again. They also
cover stable NoteId selection, active-search recomputation, cached-unavailable
retry to `Live`, failed retry preserving cached runtime, and manual-refresh timer
reset. No sleeps, threads, Tokio runtime, real Notes.app mutation, global mirror,
or offline write queue are used. A deterministic channel-gated backend proves
that UI navigation proceeds while the worker is blocked, stale results are
discarded by generation, manual refresh is visibly deferred during in-flight
work, and repeated elapsed ticks coalesce into one subsequent retry.

Phase 9 also retains one transient foreground UI intent while a refresh worker
owns the backend. The latest intent wins; after worker completion it is
revalidated against current UI state and enters the normal synchronous flow.
The intent has no NoteId or mutation payload: create opens an editor and delete
opens a fresh confirmation only. Tests cover stale-result discard before an
intent executes, selection revalidation, manual refresh handoff, due-refresh
priority, and the absence of an automatic create/delete mutation.

Phase 9 Pass A4 adds cooperative preemption for that automatic read worker.
A foreground backend intent sets the per-refresh cancellation flag; the bridge
terminates and reaps the in-flight read-only `osascript` child, returns typed
`Cancelled`, and releases the backend before the intent runs. Cancelled work
does not apply runtime/cache changes or change `DataSourceState`. Deterministic
channel-gated tests cover delete and manual-refresh handoff without sleeps;
delete still opens only a new confirmation and requires a subsequent explicit
confirmation key. No real Notes.app mutations were performed.

Phase 9 Pass A5 routes manual `r` through the same origin-tagged worker path.
The request returns immediately, UI navigation remains responsive while its
backend read is channel-gated, and UI-thread result application retains cache
persistence and warning semantics. Manual refresh preempts automatic work,
and foreground intents may preempt the manual reader; only one reader is ever
in flight. Manual requests reset the periodic interval and do not cause an
immediate redundant automatic retry.

Phase 9 Pass B1/B2 moves normal existing-note UPDATE and CREATE save to a short-lived worker. The
worker performs only conflict read/update work and returns a typed saved,
conflict, or failure outcome; the UI thread applies successful notes through
`finish_saved`. Dirty failed/conflicting editors receive no cache write.

Phase 9 Pass B3 routes only the explicit `o` action from `Popup::Conflict`
through that same worker. Detection never overwrites automatically; a failed
overwrite retains the dirty editor, while successful reconciliation still uses
`finish_saved` on the UI thread.

Phase 9 Pass B4 dispatches only confirmed move-popup destinations to the same
worker. A successful result reloads the source context without cache
read-through before persisting the authoritative moved full Note and resulting
runtime snapshot; backend failure leaves runtime and cache unchanged.

Phase 9 Pass B5 routes only explicit delegated-delete confirmation through the
same worker. Successful deletion remains UI-thread reconciled: source reload,
stable selection normalization, exact `remove_note`, and snapshot persistence.
Failures leave runtime/cache unchanged; cache failures are warning-only and do
not roll back the delegated delete.

Phase 9 Pass B6 routes explicit attachment preview and export through the
shared worker. Popup navigation stays local; completion only reports preview or
export status and makes no Notes-cache write or attachment-binary cache.

## Phase 12 A1 — safe account-scoped folder creation

`create-folder` is a narrow AppleScript bridge operation: it passes the target
stable account ID and user-entered name as script arguments, creates a
top-level account folder, and returns the authoritative `Folder`/`FolderId`.
The normal test suite performs no real Notes.app mutation. The TUI `N` popup
rejects empty or whitespace-only names, accepts Unicode, and dispatches only
after Enter through the single foreground worker. No speculative folder is
inserted. On success the UI reloads the target account's folder list, selects
the returned stable ID (not its display name), clears local search, and writes
the normal session/cache snapshot. A backend failure leaves folder/session/cache
state unchanged. Nested, rename, and delete folder operations remain out of
scope.

The completion coverage additionally proves that an authoritative backend
failure restores the exact Unicode popup input and releases the worker for a
retry without changing runtime, session, cache, search, selection, or source
state. Once creation succeeds, cache persistence failure is warning-only: the
created folder remains selected and no duplicate create is issued. No real
Notes.app mutation is used by these tests.

`successful_create_folder_session_failure_is_warning_only` injects the local
session write failure only after the backend returns the created folder. It
proves the returned ID remains selected, the popup and worker stay closed/idle,
search remains inactive, `DataSourceState` remains `Live`, the prior on-disk
session is preserved, and extra polling cannot replay creation.

## Phase 12 A2 — safe folder rename

Rename uses stable account/folder IDs and separately transported script
arguments. The foreground worker preserves the old runtime name until the
authoritative result arrives; failure restores the exact Unicode retry input.
Success applies the returned folder by ID, preserves current-folder search,
selection, focus, and preview scroll, then persists the existing derived
snapshot (which includes folder metadata). Session state stores stable IDs and
is not rewritten by rename. The bridge runner-level regressions
`rename_folder_rejects_returned_folder_id_mismatch` and
`rename_folder_rejects_returned_account_id_mismatch` reject a successful wire
response whose authoritative identity differs from the request. AppleScript
resolves the target folder inside the requested account; the mock regression
`mock_rename_folder_rejects_cross_account_target` proves a folder in another
account cannot be renamed through that request.

`successful_folder_rename_cache_failure_does_not_retry_or_rollback` injects
`ReplaceSnapshot` after authoritative backend success. It proves the renamed
folder, stable selection, selected note, active query, preview scroll, and focus
remain intact; the popup closes, the worker becomes idle, `DataSourceState`
remains `Live`, and the visible status is a cache warning rather than a rename
failure. Extra polling does not repeat the backend rename. The companion
`successful_folder_rename_does_not_rewrite_session_when_continuity_state_is_unchanged`
proves the session bytes do not change. No real Notes.app mutation is used by
these tests.

## Phase 12 A3.1 — folder-delete backend foundation

`delete-folder` is not exposed by the TUI. Its backend request contains only the
stable account and folder IDs. AppleScript resolves the account first, then finds
the folder recursively only below that account. It counts direct notes and child
folders before `delete folderRef`; any non-empty or parent folder is rejected.
This conservative policy avoids an unverified child-folder cascade. Apple’s
documented UI behavior is that notes in a deleted folder move to Recently
Deleted for 30 days; normal tests perform no destructive Notes.app action.

`mock_delete_folder_removes_exact_stable_id_target`,
`mock_delete_folder_rejects_cross_account_target`, and
`mock_delete_folder_missing_or_non_empty_target_fails` prove exact-ID targeting,
account ownership, missing-target failure, and empty-only semantics. Bridge tests
prove separate `delete-folder`, account-ID, folder-ID argument transport;
returned account/folder identity validation; and typed probe-error mapping.

The A3.2 TUI lifecycle uses navigation-focus `D` followed by explicit `y`/`Y`.
`delete_folder_shortcut_opens_confirmation_for_selected_folder` and
`delete_folder_confirmation_cancel_is_local_noop` prove no speculative backend
work. `delete_folder_runs_in_foreground_worker_without_speculative_removal`
uses a channel-gated worker and proves the folder, selection, note, scroll, and
cache remain unchanged until backend success. Success removes the exact stable
folder ID, clears old-context search, resets preview context, and persists the
derived snapshot/session; cache failure remains a visible warning without retry
or rollback. `successful_last_folder_delete_enters_empty_context` covers the
empty-navigation case. No real Notes.app folder deletion is performed.

`successful_folder_delete_removes_exact_stable_id_and_selects_next_context`,
`failed_folder_delete_preserves_authoritative_state`, and
`successful_folder_delete_session_failure_is_warning_only` additionally prove
exact-ID removal, failure-without-speculative runtime or cache changes, stable
post-delete context normalization, and warning-only session persistence. A
folder-delete intent requested while a read refresh owns the backend is queued
as `BeginDeleteFolder`, then opens only the normal confirmation after the
refresh releases ownership; it never authorizes deletion by itself.

## Phase 12 A4.1 — nested hierarchy backend foundation

`FolderParent::{Account, Folder}` carries stable hierarchy identity and the
recursive folder probe serializes every descendant with account and parent IDs.
`CachedState.folders` stores the same `Folder` values; existing navigation is
depth-first and flattened, with no new hierarchy UI.

`CreateChildFolder` and `ReparentFolder` are account-scoped stable-ID requests.
`create-child-folder ACCOUNT_ID PARENT_FOLDER_ID NAME` and
`reparent-folder ACCOUNT_ID SOURCE_FOLDER_ID TARGET_KIND TARGET_ID` carry every
argument separately. The root target is explicit `account-root` plus account
ID, never a sentinel folder ID. Bridge responses validate account, source, and
parent identity; probe errors remain typed backend errors. Mock tests reject
cross-account parents and self/descendant cycles while preserving source ID.

The Notes scripting definition confirms folders can contain folders and exposes
standard create/move commands. Shared/special restrictions, ordering, and
cross-account runtime behaviour are not inferred; cross-account reparent stays
unsupported. `manual_create_child_folder_probe` and
`manual_reparent_folder_probe` are destructive ignored tests and were not run.

## Phase 12 A4.2a — child-folder TUI creation

Navigation-focus `C` opens a typed child-folder popup only for a selected
parent folder. It carries stable account/parent IDs plus display-only parent
name and Unicode-safe input. The existing foreground worker is the sole
mutation lane: before success neither runtime hierarchy, cache, nor session is
changed. Failure restores parent/input/cursor exactly. Success selects the
authoritative returned child ID after depth-first navigation rebuild, clears
search, empties note selection, resets preview scroll, and persists the normal
snapshot/session. Tests cover stable parent targeting, empty-name rejection,
Unicode, retry preservation, and no real Notes hierarchy mutation.

Final proof coverage uses explicit mpsc entered/release gates:
`create_child_folder_runs_in_foreground_worker_without_speculative_insertion`
proves one backend call, a returned UI dispatch, unchanged hierarchy/context,
and zero mutation-time cache/session writes before release. On completion the
authoritative child `FolderId` is selected. The session-failure regression,
`successful_child_folder_create_session_failure_is_warning_only`, retains that
child and warning-only status without rollback or an extra poll retry.
`queued_child_folder_create_intent_opens_popup_without_creating` proves `C`
during a blocked read refresh queues only `BeginCreateChildFolder`; after the
refresh it opens the stable-parent popup, with no backend create until a new
explicit Enter. Ignored manual hierarchy probes remain unexecuted.

## Phase 12 A4.2b — same-account folder reparent TUI

Navigation-focus `M` opens a typed reparent popup with `AccountRoot` and stable
same-account folder targets. Source/descendant targets are excluded by stable
parent-ID traversal; current-parent confirmation is a no-op. The shared worker
performs only `reparent_folder`, and UI reconciliation replaces the returned
folder by ID, rebuilds depth-first navigation, retains selection/context, then
persists the normal derived snapshot. Reparent needs no session rewrite because
continuity IDs remain unchanged. Deterministic mpsc tests prove no speculation,
failure popup restoration, typed root, duplicate-name target IDs, cache-warning
semantics, and refresh intent opening only the popup. Manual reparent probes
remain ignored and unexecuted.

## Phase 13 A1 — TUI UX and state-machine audit

The production keymap distinguishes case: `n` creates a note, `N` opens
top-level-folder creation, `C` opens child-folder creation, `R` rename, `M`
same-account reparent, and `m` note move. `D` opens folder deletion only from
navigation when no note is selected; otherwise it retains delegated note-delete
handling. `r` refreshes, `/` edits search, `a` attachments, `,` settings, `?`
help, `q` quit, and `Tab`/`Shift-Tab` changes browsing focus. Popups own their
input: Escape cancels, text popups require Enter, destination popups require a
fresh Enter, and delete requires a fresh `y`/`Y`.

| Operation | Folder/context result | Search / preview | Session |
| --- | --- | --- | --- |
| Folder rename / reparent | Same stable folder and note context | Preserved | No rewrite when IDs unchanged |
| Folder create / child create | New folder selected | Cleared / reset | Written |
| Folder delete | Replacement or empty folder context | Cleared / reset | Written |
| Note move / delete | Current source context reconciled | Recomputed / normalized | Existing mutation policy |

The audit found documentation drift only: the help screen lacked the completed
folder controls, and README still described nested creation/reparenting as
unimplemented. Focused regressions prove case-sensitive routing and that editor
mode absorbs folder shortcuts without opening popups or workers. All foreground
mutations share one worker slot; queued refresh intents retain stable IDs and
open only the next authorization UI. Folder authority uses `AccountId` plus
`FolderId`; names are display/status-only. The flattened hierarchy builder has
visited-ID protection, so malformed parent cycles cannot recurse indefinitely.
Post-success cache/session warnings remain local and do not reclassify an
authoritative backend mutation as failed or replay it.

## Phase 13 A2 — release readiness audit

`cargo build --workspace --release --offline` produced arm64 Mach-O
`apple-notes-tui` and `notes-probe` artifacts. The user-facing TUI is
self-contained with respect to the repository: `notes-bridge` embeds the one
canonical AppleScript at compile time and invokes `/usr/bin/osascript` directly;
it neither launches nor resolves the diagnostic `notes-probe` executable. The
developer-only probe still requires its neighboring `scripts/` directory when
distributed as a probe artifact.

Safe external-working-directory smoke checks exercised `--help`, `--version`,
`--cache-info`, and `--draft-info`; these routes exit before terminal and
Notes-backend initialization. The help text now lists `--version` and all local
maintenance commands. The cache command may report a local filesystem error
when Application Support is unavailable, but does not invoke Notes.app. No real
Notes.app probe or mutation was run for this audit.

The supported release model is a source release build; there is currently no
installer, Homebrew formula, or published crates.io package. Local files are
confined to `~/Library/Application Support/apple-notes-tui/`, with the cache
derived and disposable and recovery drafts explicitly called out as containing
possible unsaved text.

### License governance

Root `LICENSE` is present and contains the canonical MIT text with the
project-owner-supplied attribution `Copyright (c) 2026 Dmytro Shvedchenko`.
All five workspace packages (`notes-core`, `notes-bridge`, `notes-cache`,
`notes-tui`, and `notes-probe`) consistently declare `license = "MIT"`; none
declares `license-file` or `authors`. The root license and Cargo SPDX
declarations therefore agree.

`repository` and `homepage` remain deliberately unset because no canonical URL
is stated in repository-owned material. `rust-version` remains unspecified:
there is no documented MSRV, CI matrix, or Rust toolchain pin that could make a
contractual MSRV defensible. These metadata omissions are nonblocking. Package
versions remain `0.1.0`; package descriptions remain accurate; no source,
schema, dependency, or version change is needed for this governance closure.

Phase 13 A2.1 is ready for a separate version/tag/release preparation pass.

## Phase 14 A1 — version and local release preparation

The first public release remains `0.1.0`: every workspace package and the TUI
version output already use that version, while completed internal scope does not
by itself establish a public `1.0.0` commitment. The expected local annotated
tag is `v0.1.0` and targets the release HEAD.

Pre-commit audit found no credentials or private-key material in the source
candidate; password/secret matches are Notes.app capability terms or test
markers. The existing local `AGENTS.md` is AI-assistant-only development
instruction and remains physically present but ignored. The ignore policy also
excludes generated Cargo targets, the old local `notes-probe` Mach-O artifact,
macOS/editor junk, and no runtime source assets. No other AI-assistant
configuration was present.

The source-release inventory retains `Cargo.lock`, `LICENSE`, `README.md`, all
crate sources, verification material, and the canonical embedded AppleScript.
There is no remote publication in this preparation scope. After the committed
tree and local tag are verified, the archive must exclude local AI instruction
files and generated artifacts while still building offline from the embedded
AppleScript source.
