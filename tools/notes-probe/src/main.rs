use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const WRITE_ACK: &str = "I_UNDERSTAND_NOTES_WILL_CHANGE";
const SCHEMA_VERSION: &str = "apple-notes-probe/v1";
const OSASCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const CANONICAL_SCRIPT: &str = include_str!("../scripts/notes_probe.applescript");

const USAGE: &str = r#"notes-probe — safe Apple Notes AppleScript capability probe

USAGE:
  notes-probe accounts
  notes-probe folders [--account-id ID]
  notes-probe notes [--account-id ID | --folder-id ID] [--limit N]
  notes-probe get-note --note-id ID
  notes-probe preview --note-id ID
  notes-probe lookup --note-id ID
  notes-probe metadata --note-id ID
  notes-probe body --note-id ID
  notes-probe plaintext --note-id ID
  notes-probe body-plaintext --note-id ID
  notes-probe attachment-count --note-id ID
  notes-probe preview-stage-metadata --note-id ID
  notes-probe preview-stage-body --note-id ID
  notes-probe preview-stage-plaintext --note-id ID
  notes-probe preview-stage-body-plaintext --note-id ID
  notes-probe preview-stage-note-serialized --note-id ID
  notes-probe preview-stage-attachments --note-id ID
  notes-probe preview-stage-full --note-id ID
  notes-probe foundation-graph-only
  notes-probe foundation-serialize-only
  notes-probe foundation-full-local
  notes-probe preview-meta-id --note-id ID
  notes-probe preview-meta-name --note-id ID
  notes-probe preview-meta-account-folder --note-id ID
  notes-probe preview-meta-created --note-id ID
  notes-probe preview-meta-modified --note-id ID
  notes-probe preview-meta-protection --note-id ID
  notes-probe preview-meta-shared --note-id ID
  notes-probe preview-meta-all --note-id ID
  notes-probe preview-meta-properties --note-id ID
  notes-probe preview-meta-properties-shape --note-id ID
  notes-probe preview-meta-properties-all --note-id ID
  notes-probe preview-properties-full --note-id ID
  notes-probe lookup-contextual --note-id ID --account-id ID --folder-id ID
  notes-probe preview-contextual --note-id ID --account-id ID --folder-id ID
  notes-probe attachments --note-id ID
  notes-probe preview-attachment --note-id ID --attachment-id ID
  notes-probe snapshot [--limit N]

  notes-probe create-note --folder-id ID --name NAME
                          (--body-html HTML | --body-file PATH)
                          [WRITE GUARD]
  notes-probe update-note --note-id ID [--name NAME]
                          [--body-html HTML | --body-file PATH]
                          [WRITE GUARD]
  notes-probe move-note --note-id ID --folder-id ID [WRITE GUARD]
  notes-probe delete-note --note-id ID --confirm-note-id ID [WRITE GUARD]

WRITE GUARD:
  Without --execute, mutation commands only emit a JSON dry-run description.
  To change Notes.app, pass both:
    --execute --write-ack I_UNDERSTAND_NOTES_WILL_CHANGE

NOTES:
  --limit 0 means no limit. The default list/snapshot limit is 50.
  All output, including local validation failures, uses apple-notes-probe/v1 JSON.
"#;

#[derive(Debug)]
struct Cli {
    operation: String,
    values: HashMap<String, String>,
    flags: HashSet<String>,
}

impl Cli {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let _program = args.next();
        let Some(operation) = args.next() else {
            return Ok(None);
        };
        if operation == "help" || operation == "--help" || operation == "-h" {
            return Ok(None);
        }

        let mut values = HashMap::new();
        let mut flags = HashSet::new();
        let remaining: Vec<String> = args.collect();
        let mut index = 0;
        while index < remaining.len() {
            let key = &remaining[index];
            if !key.starts_with("--") {
                return Err(format!("unexpected positional argument: {key}"));
            }
            if key == "--execute" {
                if !flags.insert(key.clone()) {
                    return Err(format!("duplicate option: {key}"));
                }
                index += 1;
                continue;
            }
            let Some(value) = remaining.get(index + 1) else {
                return Err(format!("missing value for {key}"));
            };
            if values.insert(key.clone(), value.clone()).is_some() {
                return Err(format!("duplicate option: {key}"));
            }
            index += 2;
        }

        Ok(Some(Self {
            operation,
            values,
            flags,
        }))
    }

    fn optional(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.optional(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("missing required option: {name}"))
    }

    fn has_flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn reject_unknown(
        &self,
        allowed_values: &[&str],
        allowed_flags: &[&str],
    ) -> Result<(), String> {
        for key in self.values.keys() {
            if !allowed_values.contains(&key.as_str()) {
                return Err(format!("unknown option for {}: {key}", self.operation));
            }
        }
        for key in &self.flags {
            if !allowed_flags.contains(&key.as_str()) {
                return Err(format!("unknown flag for {}: {key}", self.operation));
            }
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    let cli = match Cli::parse(env::args()) {
        Ok(Some(cli)) => cli,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => return local_error("cli", "invalid_arguments", &message),
    };

    match prepare(&cli) {
        Ok(Prepared::DryRun(data)) => {
            println!(
                "{{\"schemaVersion\":{},\"operation\":{},\"ok\":true,\"dryRun\":true,\"data\":{data}}}",
                json_string(SCHEMA_VERSION),
                json_string(&cli.operation)
            );
            ExitCode::SUCCESS
        }
        Ok(Prepared::Invoke(arguments)) => invoke_osascript(&cli.operation, &arguments),
        Err(message) => local_error(&cli.operation, "invalid_arguments", &message),
    }
}

enum Prepared {
    DryRun(String),
    Invoke(Vec<String>),
}

fn prepare(cli: &Cli) -> Result<Prepared, String> {
    match cli.operation.as_str() {
        "accounts" => {
            cli.reject_unknown(&[], &[])?;
            Ok(invoke(vec!["accounts"]))
        }
        "folders" => {
            cli.reject_unknown(&["--account-id"], &[])?;
            Ok(invoke(vec![
                "folders",
                cli.optional("--account-id").unwrap_or(""),
            ]))
        }
        "notes" => {
            cli.reject_unknown(&["--account-id", "--folder-id", "--limit"], &[])?;
            if cli.optional("--account-id").is_some() && cli.optional("--folder-id").is_some() {
                return Err("--account-id and --folder-id are mutually exclusive".into());
            }
            let limit = parse_limit(cli.optional("--limit"))?;
            Ok(invoke(vec![
                "notes",
                cli.optional("--account-id").unwrap_or(""),
                cli.optional("--folder-id").unwrap_or(""),
                &limit.to_string(),
            ]))
        }
        "get-note" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec!["get-note", cli.required("--note-id")?]))
        }
        "preview" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec!["preview", cli.required("--note-id")?]))
        }
        "lookup" | "metadata" | "body" | "plaintext" | "body-plaintext" | "attachment-count" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec![
                cli.operation.as_str(),
                cli.required("--note-id")?,
            ]))
        }
        "foundation-graph-only" | "foundation-serialize-only" | "foundation-full-local" => {
            cli.reject_unknown(&[], &[])?;
            Ok(invoke(vec![cli.operation.as_str()]))
        }
        "preview-meta-id"
        | "preview-meta-name"
        | "preview-meta-account-folder"
        | "preview-meta-created"
        | "preview-meta-modified"
        | "preview-meta-protection"
        | "preview-meta-shared"
        | "preview-meta-all"
        | "preview-meta-properties" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec![
                cli.operation.as_str(),
                cli.required("--note-id")?,
            ]))
        }
        "preview-meta-properties-shape"
        | "preview-meta-properties-all"
        | "preview-properties-full" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec![
                cli.operation.as_str(),
                cli.required("--note-id")?,
            ]))
        }
        "lookup-contextual" | "preview-contextual" => {
            cli.reject_unknown(&["--note-id", "--account-id", "--folder-id"], &[])?;
            Ok(invoke(vec![
                cli.operation.as_str(),
                cli.required("--note-id")?,
                cli.required("--account-id")?,
                cli.required("--folder-id")?,
            ]))
        }
        "preview-stage-metadata"
        | "preview-stage-body"
        | "preview-stage-plaintext"
        | "preview-stage-body-plaintext"
        | "preview-stage-note-serialized"
        | "preview-stage-attachments"
        | "preview-stage-full" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec![
                cli.operation.as_str(),
                cli.required("--note-id")?,
            ]))
        }
        "attachments" => {
            cli.reject_unknown(&["--note-id"], &[])?;
            Ok(invoke(vec!["attachments", cli.required("--note-id")?]))
        }
        "preview-attachment" => {
            cli.reject_unknown(&["--note-id", "--attachment-id"], &[])?;
            Ok(invoke(vec![
                "preview-attachment",
                cli.required("--note-id")?,
                cli.required("--attachment-id")?,
            ]))
        }
        "snapshot" => {
            cli.reject_unknown(&["--limit"], &[])?;
            let limit = parse_limit(cli.optional("--limit"))?;
            Ok(invoke(vec!["snapshot", &limit.to_string()]))
        }
        "create-note" => {
            cli.reject_unknown(
                &[
                    "--folder-id",
                    "--name",
                    "--body-html",
                    "--body-file",
                    "--write-ack",
                ],
                &["--execute"],
            )?;
            let folder_id = cli.required("--folder-id")?;
            let name = cli.required("--name")?;
            let body = read_body(cli)?;
            if !write_enabled(cli)? {
                return Ok(Prepared::DryRun(format!(
                    "{{\"action\":\"create-note\",\"folderId\":{},\"name\":{},\"bodyBytes\":{}}}",
                    json_string(folder_id),
                    json_string(name),
                    body.len()
                )));
            }
            Ok(invoke_owned(vec![
                "create-note".into(),
                folder_id.into(),
                name.into(),
                body,
            ]))
        }
        "update-note" => {
            cli.reject_unknown(
                &[
                    "--note-id",
                    "--name",
                    "--body-html",
                    "--body-file",
                    "--write-ack",
                ],
                &["--execute"],
            )?;
            let note_id = cli.required("--note-id")?;
            let has_name = cli.optional("--name").is_some();
            let has_body =
                cli.optional("--body-html").is_some() || cli.optional("--body-file").is_some();
            if !has_name && !has_body {
                return Err("update-note requires --name, --body-html, or --body-file".into());
            }
            let name = cli.optional("--name").unwrap_or("");
            let body = if has_body {
                read_body(cli)?
            } else {
                String::new()
            };
            if !write_enabled(cli)? {
                return Ok(Prepared::DryRun(format!(
                    "{{\"action\":\"update-note\",\"noteId\":{},\"changesName\":{},\"changesBody\":{},\"bodyBytes\":{}}}",
                    json_string(note_id), has_name, has_body, body.len()
                )));
            }
            Ok(invoke_owned(vec![
                "update-note".into(),
                note_id.into(),
                name.into(),
                body,
                bool_arg(has_name).into(),
                bool_arg(has_body).into(),
            ]))
        }
        "move-note" => {
            cli.reject_unknown(&["--note-id", "--folder-id", "--write-ack"], &["--execute"])?;
            let note_id = cli.required("--note-id")?;
            let folder_id = cli.required("--folder-id")?;
            if !write_enabled(cli)? {
                return Ok(Prepared::DryRun(format!(
                    "{{\"action\":\"move-note\",\"noteId\":{},\"destinationFolderId\":{}}}",
                    json_string(note_id),
                    json_string(folder_id)
                )));
            }
            Ok(invoke(vec!["move-note", note_id, folder_id]))
        }
        "delete-note" => {
            cli.reject_unknown(
                &["--note-id", "--confirm-note-id", "--write-ack"],
                &["--execute"],
            )?;
            let note_id = cli.required("--note-id")?;
            let confirmed_id = cli.required("--confirm-note-id")?;
            if note_id != confirmed_id {
                return Err("--confirm-note-id must exactly match --note-id".into());
            }
            if !write_enabled(cli)? {
                return Ok(Prepared::DryRun(format!(
                    "{{\"action\":\"delete-note\",\"noteId\":{},\"warning\":\"Deletion is delegated to Notes.app\"}}",
                    json_string(note_id)
                )));
            }
            Ok(invoke(vec!["delete-note", note_id]))
        }
        other => Err(format!("unknown operation: {other}")),
    }
}

fn invoke(values: Vec<&str>) -> Prepared {
    Prepared::Invoke(values.into_iter().map(str::to_owned).collect())
}

fn invoke_owned(values: Vec<String>) -> Prepared {
    Prepared::Invoke(values)
}

fn parse_limit(value: Option<&str>) -> Result<u32, String> {
    let value = value.unwrap_or("50");
    value
        .parse::<u32>()
        .map_err(|_| format!("--limit must be a non-negative integer, got: {value}"))
}

fn read_body(cli: &Cli) -> Result<String, String> {
    match (cli.optional("--body-html"), cli.optional("--body-file")) {
        (Some(_), Some(_)) => Err("--body-html and --body-file are mutually exclusive".into()),
        (Some(body), None) => Ok(body.to_owned()),
        (None, Some(path)) => fs::read_to_string(Path::new(path))
            .map_err(|error| format!("could not read --body-file {path:?}: {error}")),
        (None, None) => Err("one of --body-html or --body-file is required".into()),
    }
}

fn write_enabled(cli: &Cli) -> Result<bool, String> {
    if !cli.has_flag("--execute") {
        return Ok(false);
    }
    match cli.optional("--write-ack") {
        Some(value) if value == WRITE_ACK => Ok(true),
        _ => Err(format!("--execute requires --write-ack {WRITE_ACK}")),
    }
}

fn bool_arg(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn invoke_osascript(operation: &str, arguments: &[String]) -> ExitCode {
    if !cfg!(target_os = "macos") {
        return local_error(
            operation,
            "unsupported_platform",
            "notes-probe requires macOS",
        );
    }
    let script_path = match runtime_script_path() {
        Ok(path) if path.is_file() => path,
        Ok(path) => {
            return local_error(
                operation,
                "missing_script",
                &format!("AppleScript not found at {}", path.display()),
            )
        }
        Err(message) => return local_error(operation, "missing_script", &message),
    };

    let mut child = match Command::new("/usr/bin/osascript")
        .arg(&script_path)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return local_error(operation, "osascript_launch_failed", &error.to_string()),
    };
    let Some(mut child_stdout) = child.stdout.take() else {
        return local_error(
            operation,
            "osascript_pipe_failed",
            "stdout pipe is unavailable",
        );
    };
    let Some(mut child_stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return local_error(
            operation,
            "osascript_pipe_failed",
            "stderr pipe is unavailable",
        );
    };
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < OSASCRIPT_TIMEOUT => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                let message = if is_mutation(operation) {
                    "osascript timed out after 30 seconds; Notes.app state may be indeterminate"
                        .to_owned()
                } else {
                    format!("osascript timed out after 30 seconds while executing {operation}")
                };
                return local_error(operation, "osascript_timeout", &message);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return local_error(operation, "osascript_wait_failed", &error.to_string());
            }
        }
    };
    let stdout = match stdout_reader.join() {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).trim().to_owned(),
        Ok(Err(error)) => {
            return local_error(operation, "osascript_output_failed", &error.to_string())
        }
        Err(_) => {
            return local_error(
                operation,
                "osascript_output_failed",
                "stdout reader panicked",
            )
        }
    };
    let stderr = match stderr_reader.join() {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).trim().to_owned(),
        Ok(Err(error)) => {
            return local_error(operation, "osascript_output_failed", &error.to_string())
        }
        Err(_) => {
            return local_error(
                operation,
                "osascript_output_failed",
                "stderr reader panicked",
            )
        }
    };

    if !status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return local_error(operation, "osascript_failed", &detail);
    }
    if !stdout.starts_with('{') || !stdout.ends_with('}') {
        return local_error(operation, "invalid_probe_output", &stdout);
    }

    println!("{stdout}");
    if stdout.contains("\"ok\":false") {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn runtime_script_path() -> Result<PathBuf, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("could not determine current executable: {error}"))?;
    script_path_from_executable(&executable)
}

fn script_path_from_executable(executable: &Path) -> Result<PathBuf, String> {
    let executable_dir = executable.parent().ok_or_else(|| {
        format!(
            "current executable has no parent directory: {}",
            executable.display()
        )
    })?;
    Ok(executable_dir
        .join("scripts")
        .join("notes_probe.applescript"))
}

fn is_mutation(operation: &str) -> bool {
    matches!(
        operation,
        "create-note" | "update-note" | "move-note" | "delete-note"
    )
}

fn local_error(operation: &str, code: &str, message: &str) -> ExitCode {
    println!(
        "{{\"schemaVersion\":{},\"operation\":{},\"ok\":false,\"error\":{{\"source\":\"runner\",\"code\":{},\"message\":{}}}}}",
        json_string(SCHEMA_VERSION),
        json_string(operation),
        json_string(code),
        json_string(message)
    );
    ExitCode::FAILURE
}

fn json_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{08}' => result.push_str("\\b"),
            '\u{0c}' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c <= '\u{1f}' => {
                use std::fmt::Write;
                let _ = write!(result, "\\u{:04x}", c as u32);
            }
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse(args.iter().map(|value| value.to_string()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn escapes_json_control_characters() {
        assert_eq!(json_string("a\n\"b\\c\t"), "\"a\\n\\\"b\\\\c\\t\"");
    }

    #[test]
    fn mutations_are_dry_run_by_default() {
        let cli = parse(&[
            "notes-probe",
            "move-note",
            "--note-id",
            "note-1",
            "--folder-id",
            "folder-1",
        ]);
        assert!(matches!(prepare(&cli), Ok(Prepared::DryRun(_))));
    }

    #[test]
    fn writes_require_exact_acknowledgement() {
        let cli = parse(&[
            "notes-probe",
            "move-note",
            "--note-id",
            "note-1",
            "--folder-id",
            "folder-1",
            "--execute",
            "--write-ack",
            "wrong",
        ]);
        assert!(prepare(&cli).is_err());
    }

    #[test]
    fn delete_requires_matching_note_id() {
        let cli = parse(&[
            "notes-probe",
            "delete-note",
            "--note-id",
            "note-1",
            "--confirm-note-id",
            "note-2",
        ]);
        assert!(prepare(&cli).is_err());
    }

    #[test]
    fn attachment_preview_requires_both_opaque_ids_as_separate_arguments() {
        let cli = parse(&[
            "notes-probe",
            "preview-attachment",
            "--note-id",
            "note;not-shell",
            "--attachment-id",
            "attachment $(not-shell)",
        ]);
        let Prepared::Invoke(arguments) = prepare(&cli).unwrap() else {
            panic!("preview must invoke the read-only script operation");
        };
        assert_eq!(
            arguments,
            vec![
                "preview-attachment",
                "note;not-shell",
                "attachment $(not-shell)"
            ]
        );
        assert!(!is_mutation("preview-attachment"));
    }

    #[test]
    fn diagnostic_cli_operations_have_canonical_applescript_dispatch() {
        for operation in [
            "lookup",
            "metadata",
            "body",
            "plaintext",
            "body-plaintext",
            "attachment-count",
            "preview-stage-metadata",
            "preview-stage-body",
            "preview-stage-plaintext",
            "preview-stage-body-plaintext",
            "preview-stage-note-serialized",
            "preview-stage-attachments",
            "preview-stage-full",
            "foundation-graph-only",
            "foundation-serialize-only",
            "foundation-full-local",
            "preview-meta-id",
            "preview-meta-name",
            "preview-meta-account-folder",
            "preview-meta-created",
            "preview-meta-modified",
            "preview-meta-protection",
            "preview-meta-shared",
            "preview-meta-all",
            "preview-meta-properties",
            "preview-meta-properties-shape",
            "preview-meta-properties-all",
            "preview-properties-full",
            "lookup-contextual",
            "preview-contextual",
        ] {
            let prefix = if operation.starts_with("preview-stage-") {
                "operationName starts with \"preview-stage-\""
            } else {
                "operationName starts with \"preview-meta-\""
            };
            assert!(
                CANONICAL_SCRIPT.contains(&format!("operationName is \"{operation}\""))
                    || (operation.starts_with("preview-stage-")
                        || operation.starts_with("preview-meta-"))
                        && CANONICAL_SCRIPT.contains(prefix),
                "missing AppleScript dispatcher coverage for {operation}",
            );
        }
    }

    #[test]
    fn build_script_provisions_canonical_runtime_sidecar() {
        let build_script = include_str!("../build.rs");
        assert!(build_script.contains("cargo:rerun-if-changed=scripts/notes_probe.applescript"));
        assert!(build_script.contains("copy(&source, &destination)"));
        assert!(build_script.contains("join(\"scripts\")"));
    }

    #[test]
    fn foundation_local_diagnostics_use_native_object_graph_path() {
        assert!(CANONICAL_SCRIPT.contains("on fixedFoundationGraph()"));
        assert!(CANONICAL_SCRIPT.contains("on foundationGraphOnly()"));
        assert!(CANONICAL_SCRIPT.contains("on foundationSerializeOnly()"));
        assert!(CANONICAL_SCRIPT.contains("on foundationFullLocal()"));
        assert!(CANONICAL_SCRIPT.contains("NSMutableArray's array()"));
        assert!(CANONICAL_SCRIPT.contains("NSJSONSerialization's dataWithJSONObject"));
    }

    #[test]
    fn production_preview_serializes_one_foundation_root() {
        let preview = CANONICAL_SCRIPT
            .split("on probePreview(noteId)")
            .nth(1)
            .and_then(|body| body.split("end probePreview").next())
            .expect("preview handler");
        assert!(preview.contains("return my foundationJson(rootItem)"));
        assert_eq!(preview.matches("foundationJson(").count(), 1);
        assert!(!preview.contains("jsonString(note"));
    }

    #[test]
    fn resolves_script_relative_to_executable_directory() {
        let executable = Path::new("/opt/apple-notes-tui/notes-probe");
        let script = script_path_from_executable(executable).unwrap();
        assert_eq!(
            script,
            PathBuf::from("/opt/apple-notes-tui/scripts/notes_probe.applescript")
        );
    }
}
