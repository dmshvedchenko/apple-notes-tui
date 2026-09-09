# Changelog

## 0.1.1 - 2026-09-09

### Changed

- Cached Notes data is shown immediately while live Notes.app reconciliation runs
  in the background.
- Folder and note navigation use cached data immediately and refresh
  asynchronously; rapid navigation coalesces obsolete reads.
- Previously visited folder summaries are retained during the session.

### Fixed

- Reduced UI stalls during long Apple Events reads and prevented stale reads
  from overwriting newer selections.

### Diagnostics

- Added opt-in privacy-safe tracing with `APPLE_NOTES_TUI_PERF=1`.

## 0.1.0 - 2026-09-09

### Highlights

- macOS Apple Notes terminal UI with stable-ID note and folder mutations.
- Rich-text note editing plus attachment preview and export.
- Derived persistent cache, local configuration, session continuity, and editor
  recovery drafts.
- Responsive read-only refresh and foreground backend operations.
- Folder create, rename, delete, nested child creation, and same-account
  reparenting.

### Limitations

- macOS only; the validated release build targets Apple Silicon (arm64).
- No drag and drop, cross-account reparenting, recursive folder deletion, or
  undo/redo.
