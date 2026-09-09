# notes-probe

notes-probe is the Phase 0 capability probe for a future Rust bridge to
Apple Notes. The Rust runner validates arguments, enforces write guards, starts
osascript, applies a 30-second timeout, and forwards a versioned JSON result.
The AppleScript uses only the public Notes.app scripting dictionary.

It never opens NoteStore.sqlite. Notes.app remains the source of truth for
local data and iCloud synchronization.

## Requirements and permissions

- macOS with Notes.app and at least one configured Notes account.
- A Rust toolchain with Cargo.
- Automation permission for the process that launches the probe to control
  Notes. On first access, macOS may display a prompt. If access was denied,
  enable the relevant host under System Settings > Privacy & Security >
  Automation > Notes.
- In Codex or another sandboxed host, compiling or running AppleScript may also
  require permission to leave the sandbox. A compile failure that treats Notes
  terms such as account as unknown usually means the Notes scripting dictionary
  could not be loaded from that sandbox.

The probe does not require Full Disk Access because it does not access the
Notes database directly.

## Build and runner

From the repository root:

~~~sh
cargo build --workspace
./tools/notes-probe/notes-probe help
~~~

The convenience runner builds the Rust binary as needed. To invoke an already
built binary, use target/debug/notes-probe.

## Read-only probes

~~~sh
./tools/notes-probe/notes-probe accounts
./tools/notes-probe/notes-probe folders
./tools/notes-probe/notes-probe folders --account-id ACCOUNT_ID
./tools/notes-probe/notes-probe notes --limit 20
./tools/notes-probe/notes-probe notes --folder-id FOLDER_ID --limit 20
./tools/notes-probe/notes-probe get-note --note-id NOTE_ID
./tools/notes-probe/notes-probe attachments --note-id NOTE_ID
./tools/notes-probe/notes-probe preview-attachment \
  --note-id NOTE_ID --attachment-id ATTACHMENT_ID
./tools/notes-probe/notes-probe snapshot --limit 20
~~~

A limit of 0 requests every matching note. The default is 50 because large
libraries can make AppleScript slow. snapshot is the common read-only runner:
it returns accounts, the recursive folder tree, and note metadata in one
envelope. It uses one accounts-to-folders-to-notes traversal; it does not call
get-note per item or read HTML body/plaintext. Attachment contents are not read;
attachmentCount comes from the attachment groups returned with each folder's
note collection.

scripts/verify-readonly.sh runs a small read-only smoke suite and validates each
response with the JSON parser included in macOS Ruby.

## Mutating probes and safety

create-note, update-note, move-note, and delete-note are dry-run operations
unless both of these arguments are present:

~~~text
--execute --write-ack I_UNDERSTAND_NOTES_WILL_CHANGE
~~~

delete-note additionally requires confirm-note-id to exactly match note-id.
Bodies can be passed as HTML or read from a UTF-8 file, which avoids shell
quoting problems. User-controlled values are passed as osascript argv entries;
they are never interpolated into AppleScript source.

Dry-run examples:

~~~sh
./tools/notes-probe/notes-probe create-note \
  --folder-id FOLDER_ID \
  --name "Phase 0 fixture" \
  --body-file tools/notes-probe/fixtures/note-body.html

./tools/notes-probe/notes-probe delete-note \
  --note-id NOTE_ID \
  --confirm-note-id NOTE_ID
~~~

To perform an isolated lifecycle verification, use two disposable folders such
as `Apple Notes TUI Test` and `Apple Notes TUI Test Moved`. Obtain their IDs
with `folders`; create a uniquely named note only in the first folder. Run each
write as a dry run first, then add the required acknowledgement only after the
dry-run JSON names the intended object.

~~~sh
./tools/notes-probe/notes-probe create-note \
  --folder-id FOLDER_A_ID \
  --name "apple-notes-tui Phase0 UNIQUE-VALUE" \
  --body-file tools/notes-probe/fixtures/write-lifecycle-initial.html \
  --execute --write-ack I_UNDERSTAND_NOTES_WILL_CHANGE

./tools/notes-probe/notes-probe update-note \
  --note-id CREATED_NOTE_ID \
  --name "apple-notes-tui Phase0 updated UNIQUE-VALUE" \
  --body-file tools/notes-probe/fixtures/write-lifecycle-updated.html \
  --execute --write-ack I_UNDERSTAND_NOTES_WILL_CHANGE

./tools/notes-probe/notes-probe move-note \
  --note-id CREATED_NOTE_ID \
  --folder-id FOLDER_B_ID \
  --execute --write-ack I_UNDERSTAND_NOTES_WILL_CHANGE

./tools/notes-probe/notes-probe delete-note \
  --note-id CREATED_NOTE_ID \
  --confirm-note-id CREATED_NOTE_ID \
  --execute --write-ack I_UNDERSTAND_NOTES_WILL_CHANGE
~~~

Check Notes.app after every step. Deletion is delegated to Notes, so recovery
and eventual permanent deletion follow Notes.app behavior. On the verified
macOS configuration, deleting moved the note to `Recently Deleted`: `get-note`
continued to return it with that folder's ID, while the destination folder was
empty. Treat that as the equivalent successful deletion state, not a failed
delete. If a mutating osascript call times out, its final state is indeterminate;
inspect Notes.app before retrying.

## JSON contract

Every command emits one JSON object. Successful AppleScript responses use this
shape:

~~~json
{
  "schemaVersion": "apple-notes-probe/v1",
  "operation": "accounts",
  "ok": true,
  "data": []
}
~~~

Dry-run writes add dryRun=true. Failures include error.source, a string code,
and a message. The example fixtures document all three envelope variants.

IDs are opaque Notes identifiers and must not be parsed. Date values are
currently exposed as creationDateText and modificationDateText because the
AppleScript date coercion is locale-dependent. A production bridge should
normalize dates at the native boundary.

`get-note` also includes `accountId` and `folderId`. Its parent context is
resolved by traversing accounts, folders, and notes by note ID; it does not ask
Notes for `container of note`, which is unreliable for Core Data-backed IDs.

`preview-attachment` is an explicit, read-only UI action. It uses the Notes
scripting dictionary's `show attachment` command and never runs automatically
while enumerating or selecting metadata. The production TUI also supports
exporting a copy with Notes' `save attachment in file` command, but the
diagnostic CLI deliberately does not expose that file-creating operation.

## Known Phase 0 constraints

- The supported scripting dictionary exposes account, folder, note, and
  attachment metadata plus note CRUD and move. It does not expose every Notes
  GUI feature.
- Password-protected notes may reject body or plaintext reads until Notes has
  unlocked them.
- Attachment enumeration is metadata-only. File contents and private paths are
  not exposed. Explicit preview uses Notes.app; production export creates only
  a user-selected copy and never creates or mutates a Notes attachment.
- Tags, smart folders, collaboration details, scans, drawings, tables, and
  checklist semantics are not modeled by the Notes AppleScript dictionary.
- Apple Events calls are synchronous and can be slow while Notes launches or
  iCloud state converges. The runner stops waiting after 30 seconds.
- List order is the order returned by Notes.app and is not declared stable.

These limitations are probe findings, not reasons to bypass Notes.app with
direct database writes. Unsupported features will need separately evaluated,
Notes-mediated fallbacks in later phases.
