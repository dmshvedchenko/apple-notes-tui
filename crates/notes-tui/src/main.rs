use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use notes_bridge::AppleScriptNotesBackend;
use notes_cache::SqliteNotesCache;
use notes_tui::{
    clear_editor_draft, config_path, edit_config, editor_draft_path, format_editor_draft_info,
    load_editor_draft, load_session, parse_config_edit_command, parse_config_overrides,
    parse_draft_cli_command, render, resolve_config, session_path, App, DraftCliCommand,
};
use ratatui::{backend::CrosstermBackend, Terminal};

const HELP: &str = "apple-notes-tui [--demo|--help|--version|--cache-info|--cache-clear|--config-info|--draft-info|--draft-clear]\n\nConfiguration overrides: --refresh-interval <seconds>, --auto-refresh|--no-auto-refresh, --preview-wrap|--no-preview-wrap, --show-attachment-metadata|--hide-attachment-metadata\n\nConfig editing: --config-set <refresh_interval_seconds|auto_refresh|preview_wrap|show_attachment_metadata>=<value>, --config-unset <key>, --config-reset\n\nMaintenance commands: --cache-info, --cache-clear, --config-info, --draft-info, --draft-clear. Draft commands are standalone local-only operations.\n\nApple Notes terminal frontend.";

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(command) = parse_draft_cli_command(&args)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
    {
        let path = editor_draft_path();
        match command {
            DraftCliCommand::Info => {
                let output = format_editor_draft_info(&path)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                let failed = output.contains("Status: malformed")
                    || output.contains("Status: unsupported version");
                println!("{output}");
                if failed {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "draft inspection failed",
                    ));
                }
            }
            DraftCliCommand::Clear => match clear_editor_draft(&path) {
                Ok(true) => println!("Cleared recovery draft: {}", path.display()),
                Ok(false) => println!("Draft: none"),
                Err(error) => return Err(io::Error::other(error)),
            },
        }
        return Ok(());
    }
    let cli = parse_config_overrides(&args)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if let Some(command) = parse_config_edit_command(&args)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
    {
        edit_config(&config_path(), command)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        println!("Updated config: {}", config_path().display());
        return Ok(());
    }
    let option = args.first().map(String::as_str);
    match option {
        Some("--help") | Some("-h") => {
            println!("{HELP}");
            return Ok(());
        }
        Some("--version") | Some("-V") => {
            println!("apple-notes-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--cache-info") => {
            match SqliteNotesCache::open_or_recover(SqliteNotesCache::application_support_path()).and_then(|cache| cache.info()) {
                Ok(info) => println!("path: {}\nschema version: {}\naccounts: {}\nfolders: {}\nnotes: {}\nfull notes: {}\nlast successful refresh: {}", info.path.display(), info.schema_version, info.accounts, info.folders, info.notes, info.full_notes, info.last_successful_refresh.unwrap_or_else(|| "never".into())),
                Err(error) => eprintln!("cache unavailable: {error}"),
            }
            return Ok(());
        }
        Some("--cache-clear") => {
            let path = SqliteNotesCache::application_support_path();
            match SqliteNotesCache::clear_path(&path) {
                Ok(()) => println!("cleared derived cache: {}", path.display()),
                Err(error) => eprintln!("cache unavailable: {error}"),
            }
            return Ok(());
        }
        Some("--config-info") => {
            let resolved = resolve_config(&config_path(), cli)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            println!(
                "Config path: {}\nRefresh interval: {}s ({:?})\nAuto refresh: {} ({:?})\nPreview wrap: {} ({:?})\nAttachment metadata: {} ({:?})\nWarning: {}",
                config_path().display(),
                resolved.config.refresh_interval.as_secs(),
                resolved.refresh_interval_source,
                resolved.config.auto_refresh,
                resolved.auto_refresh_source,
                resolved.config.preview_wrap,
                resolved.preview_wrap_source,
                resolved.config.show_attachment_metadata,
                resolved.show_attachment_metadata_source,
                resolved.warning.unwrap_or_else(|| "none".into())
            );
            return Ok(());
        }
        Some("--demo")
        | Some("--refresh-interval")
        | Some("--auto-refresh")
        | Some("--no-auto-refresh")
        | Some("--preview-wrap")
        | Some("--no-preview-wrap")
        | Some("--show-attachment-metadata")
        | Some("--hide-attachment-metadata")
        | None => {}
        Some(other) => {
            eprintln!("unknown option: {other}\n\n{HELP}");
            return Ok(());
        }
    }
    let demo = args.iter().any(|argument| argument == "--demo");
    let cache = if demo {
        None
    } else {
        SqliteNotesCache::open_or_recover(SqliteNotesCache::application_support_path()).ok()
    };
    let resolved = resolve_config(&config_path(), cli)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut app = if demo {
        App::with_config(
            Box::new(notes_core::MockNotesBackend::default()),
            resolved.config,
        )
    } else if let Some(cache) = cache {
        App::with_cache_and_config(
            Box::new(AppleScriptNotesBackend::new()),
            Box::new(cache),
            resolved.config,
        )
    } else {
        App::with_config(Box::new(AppleScriptNotesBackend::new()), resolved.config)
    };
    if let Some(warning) = resolved.warning {
        app.status.text = warning;
    }
    app.set_config_metadata(
        config_path(),
        [
            resolved.refresh_interval_source,
            resolved.auto_refresh_source,
            resolved.preview_wrap_source,
            resolved.show_attachment_metadata_source,
        ],
        resolved.file_presence,
    );
    let session_file = session_path();
    match load_session(&session_file) {
        Ok(session) => app.set_session_state(session_file, session),
        Err(error) => {
            app.set_session_state(session_file, None);
            app.status.text = format!("Session warning: {error}");
        }
    }
    let draft_file = editor_draft_path();
    match load_editor_draft(&draft_file) {
        Ok(draft) => app.set_editor_draft_state(draft_file, draft),
        Err(error) => {
            app.set_editor_draft_state(draft_file.clone(), None);
            app.status.text = format!(
                "Editor recovery warning: {error}; file left intact at {}",
                draft_file.display()
            );
        }
    }
    app.bootstrap_cache();
    let mut terminal = TerminalSession::enter()?;
    app.refresh();
    app.offer_editor_draft_recovery();
    loop {
        app.poll_periodic_refresh();
        terminal.terminal.draw(|frame| render(frame, &app))?;
        if app.should_quit {
            break;
        }
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }
        app.periodic_refresh_at(Instant::now());
        app.poll_periodic_refresh();
    }
    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}
impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let mut cleanup = io::stdout();
                let _ = execute!(cleanup, LeaveAlternateScreen);
                Err(error)
            }
        }
    }
}
impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}
