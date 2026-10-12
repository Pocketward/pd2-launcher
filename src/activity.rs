//! Bounded, redacted record of what went wrong, kept for the saved report.
//!
//! The durable job history (`diagnostics::DiagnosticStore`) keeps only typed
//! codes. When a step fails, maintainers also need the step's failure code
//! and detail, the exit status and output tail of the Wine, installer,
//! helper, or `wineserver` process that failed, the host and HTTP status of
//! a failed download, and when each phase started. Those are appended here,
//! redacted ([`crate::redact`]), as JSON lines in
//! `logs/diagnostics/activity.jsonl`: at most [`FILE_LIMIT`] bytes, with the
//! previous file kept as `activity.1.jsonl`. Files are written atomically and
//! owner-only.
//!
//! Recording never fails the work it describes: until the Launcher opens its
//! diagnostics (and in unit tests) nothing is written.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    diagnostics::{JobKind, JobStatus},
    operation::WorkPhase,
    persistence::{atomic_write, read_bounded_regular},
    redact::Redactor,
    safe_fs::Directory,
};

pub(crate) const FILE_NAME: &str = "activity.jsonl";
pub(crate) const PREVIOUS_FILE_NAME: &str = "activity.1.jsonl";
/// One file's cap; with the previous file at most twice this is kept.
pub(crate) const FILE_LIMIT: usize = 256 * 1024;
/// The tail of each captured stream kept for a failed process.
pub(crate) const OUTPUT_TAIL: usize = 4 * 1024;
const TEXT_LIMIT: usize = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub(crate) enum ActivityEvent {
    JobStarted {
        kind: JobKind,
        phase: WorkPhase,
    },
    Phase {
        phase: WorkPhase,
    },
    JobFinished {
        kind: JobKind,
        status: JobStatus,
    },
    /// A job's stable failure code, as the result card shows it.
    JobFailed {
        code: String,
    },
    /// The installer's typed error behind a failed step.
    StageFailed {
        code: String,
        detail: String,
    },
    Process {
        label: String,
        /// `exited`, `timed-out`, `spawn-failed`, or `wait-failed`.
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stdout: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stderr: String,
    },
    Download {
        target: String,
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        error: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ActivityEntry {
    /// Seconds since the Unix epoch (UTC).
    pub(crate) at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) job: Option<u64>,
    #[serde(flatten)]
    pub(crate) event: ActivityEvent,
    /// The same entry recorded again in a row (a retried download).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub(crate) repeat: u32,
}

fn one() -> u32 {
    1
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_one(value: &u32) -> bool {
    *value == 1
}

struct State {
    directory: Option<PathBuf>,
    job: Option<u64>,
    redactor: Option<Redactor>,
}

static STATE: Mutex<State> = Mutex::new(State {
    directory: None,
    job: None,
    redactor: None,
});

/// Start recording into `directory` (`logs/diagnostics`).
pub(crate) fn configure(directory: PathBuf) {
    if let Ok(mut state) = STATE.lock() {
        state.directory = Some(directory);
        state.redactor = Some(Redactor::from_environment());
    }
}

/// The job later entries belong to.
pub(crate) fn set_job(job: Option<u64>) {
    if let Ok(mut state) = STATE.lock() {
        state.job = job;
    }
}

/// Append `event`, redacted. Errors are ignored: a full disk must not turn
/// a download failure into a different failure.
pub(crate) fn record(event: ActivityEvent) {
    record_entry(None, event);
}

/// Append `event` for `job` rather than the current job.
pub(crate) fn record_for(job: u64, event: ActivityEvent) {
    record_entry(Some(job), event);
}

/// Lines kept for the progress view's log, newest last.
const RECENT_LINES: usize = 64;
static RECENT: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// Bumped whenever a line is added, so the progress view redraws only then.
static RECENT_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The phase the running job is in, and since when.
static CURRENT_PHASE: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

pub(crate) fn recent_version() -> u64 {
    RECENT_VERSION.load(std::sync::atomic::Ordering::Relaxed)
}

/// The running job's phase and how long it has been in it.
pub(crate) fn current_phase() -> Option<(String, std::time::Duration)> {
    CURRENT_PHASE
        .lock()
        .ok()?
        .as_ref()
        .map(|(phase, since)| (phase.clone(), since.elapsed()))
}

/// `text` with what the reports mask (CD keys, the home folder, the user
/// name) masked, for everything shown or printed as it happens.
pub(crate) fn redacted(text: &str) -> String {
    live_redactor().text(text)
}

fn live_redactor() -> &'static Redactor {
    static LIVE: std::sync::OnceLock<Redactor> = std::sync::OnceLock::new();
    LIVE.get_or_init(Redactor::from_environment)
}

/// One line a running program wrote, for the progress log as it happens.
pub(crate) fn live_output(line: &str) {
    let line = redacted(line.trim_end());
    let line = line.as_str();
    if line.trim().is_empty() {
        return;
    }
    // Also on standard error, where DroidDeck's session log keeps it.
    eprintln!("pd2-launcher: | {}", bounded(line, 300));
    if let Ok(mut recent) = RECENT.lock() {
        if recent.len() == RECENT_LINES {
            recent.pop_front();
        }
        recent.push_back(format!("  {}", bounded(line, 300)));
        RECENT_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The last `count` lines of what the running job did, for the log under its
/// progress bar.
pub(crate) fn recent_lines(count: usize) -> Vec<String> {
    RECENT.lock().map_or_else(
        |_| Vec::new(),
        |recent| {
            recent
                .iter()
                .skip(recent.len().saturating_sub(count))
                .cloned()
                .collect()
        },
    )
}

/// Test seam: add one line to the progress log.
#[cfg(test)]
pub(crate) fn push_recent_for_test(line: &str) {
    if let Ok(mut recent) = RECENT.lock() {
        recent.push_back(line.to_owned());
    }
}

fn note(event: &ActivityEvent) {
    let Ok(mut recent) = RECENT.lock() else {
        return;
    };
    if matches!(event, ActivityEvent::JobStarted { .. }) {
        recent.clear();
    }
    if let Ok(mut current) = CURRENT_PHASE.lock() {
        match event {
            ActivityEvent::JobStarted { phase, .. } | ActivityEvent::Phase { phase } => {
                *current = Some((words(&format!("{phase:?}")), std::time::Instant::now()));
            }
            ActivityEvent::JobFinished { .. } => *current = None,
            _ => {}
        }
    }
    RECENT_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut push = |line: String| {
        if recent.len() == RECENT_LINES {
            recent.pop_front();
        }
        recent.push_back(line);
    };
    match event {
        ActivityEvent::JobStarted { kind, phase } => {
            push(format!(
                "{} started: {}",
                words(&format!("{kind:?}")),
                words(&format!("{phase:?}"))
            ));
        }
        ActivityEvent::Phase { phase } => push(words(&format!("{phase:?}"))),
        ActivityEvent::JobFinished { kind, status } => {
            push(format!(
                "{} {}",
                words(&format!("{kind:?}")),
                words(&format!("{status:?}")).to_lowercase()
            ));
        }
        ActivityEvent::JobFailed { code } | ActivityEvent::StageFailed { code, .. } => {
            push(format!("Failed: {code}"));
        }
        ActivityEvent::Process {
            label,
            outcome,
            exit_code,
            stdout,
            stderr,
        } => {
            push(format!(
                "{label} {outcome}{}",
                exit_code.map_or_else(String::new, |code| format!(" (exit code {code})"))
            ));
            let lines = stdout
                .lines()
                .chain(stderr.lines())
                .filter(|line| !line.trim().is_empty());
            let lines = lines.collect::<Vec<_>>();
            for line in &lines[lines.len().saturating_sub(6)..] {
                push(format!("  {line}"));
            }
        }
        ActivityEvent::Download {
            target,
            host,
            status,
            error,
        } => push(format!(
            "Download of {target} from {host} failed: {error}{}",
            status.map_or_else(String::new, |status| format!(" (HTTP {status})"))
        )),
    }
}

/// `PrefixInitialization` as "Prefix initialization".
fn words(name: &str) -> String {
    let mut text = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_uppercase() && index > 0 {
            text.push(' ');
            text.extend(character.to_lowercase());
        } else {
            text.push(character);
        }
    }
    text
}

fn record_entry(job: Option<u64>, event: ActivityEvent) {
    note(&redact_event(event.clone(), live_redactor()));
    let Ok(state) = STATE.lock() else {
        return;
    };
    let (Some(directory), Some(redactor)) = (&state.directory, &state.redactor) else {
        return;
    };
    let entry = ActivityEntry {
        at: now(),
        job: job.or(state.job),
        event: redact_event(event, redactor),
        repeat: 1,
    };
    let _ = append(directory, entry);
}

/// A failed process: its label, how it ended, and the tail of what it wrote.
pub(crate) fn record_process(
    label: &str,
    outcome: &str,
    exit_code: Option<i32>,
    output: &crate::host_execution::ProcessOutput,
) {
    // Also on standard error: on some systems (DroidDeck) that is the only
    // log anyone can read.
    eprintln!(
        "pd2-launcher: {label} {outcome} (exit code {})",
        exit_code.map_or_else(|| "none".to_owned(), |code| code.to_string())
    );
    for (stream, bytes) in [("stdout", output.stdout()), ("stderr", output.stderr())] {
        for line in tail_text(bytes, OUTPUT_TAIL)
            .lines()
            .filter(|line| !line.is_empty())
        {
            eprintln!("pd2-launcher:   {stream}: {}", redacted(line));
        }
    }
    record(ActivityEvent::Process {
        label: label.to_owned(),
        outcome: outcome.to_owned(),
        exit_code,
        stdout: tail_text(output.stdout(), OUTPUT_TAIL),
        stderr: tail_text(output.stderr(), OUTPUT_TAIL),
    });
}

/// A failed download attempt, by host only.
pub(crate) fn record_download(target: &str, url: &str, status: Option<u16>, error: &str) {
    record(ActivityEvent::Download {
        target: target.to_owned(),
        host: crate::sources::redacted_host(url),
        status,
        error: error.to_owned(),
    });
}

/// Classify a transport error without its message (which names the URL).
pub(crate) fn transport_error_kind(error: &zed_reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed-out"
    } else if error.is_connect() {
        "connection-failed"
    } else if error.is_redirect() {
        "redirect-refused"
    } else if error.is_body() || error.is_decode() {
        "response-interrupted"
    } else if error.is_request() {
        "request-failed"
    } else {
        "transport-failed"
    }
}

/// Every retained entry, oldest first.
pub(crate) fn read(directory: &Path) -> Vec<ActivityEntry> {
    [PREVIOUS_FILE_NAME, FILE_NAME]
        .into_iter()
        .flat_map(|name| read_file(&directory.join(name)))
        .collect()
}

fn read_file(path: &Path) -> Vec<ActivityEntry> {
    let Ok(bytes) = read_bounded_regular(path, 2 * FILE_LIMIT as u64) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Append one entry under `directory`, rotating a full file.
pub(crate) fn append(directory: &Path, entry: ActivityEntry) -> io::Result<()> {
    Directory::ensure_absolute(directory)?;
    let path = directory.join(FILE_NAME);
    let mut entries = read_file(&path);
    match entries.last_mut() {
        Some(last)
            if last.job == entry.job
                && last.event == entry.event
                && matches!(entry.event, ActivityEvent::Download { .. }) =>
        {
            last.repeat = last.repeat.saturating_add(1);
            last.at = entry.at;
        }
        _ => entries.push(entry),
    }
    let encoded = encode(&entries)?;
    if encoded.len() <= FILE_LIMIT {
        return atomic_write(&path, &encoded);
    }
    // Full: the older entries become the previous file.
    let latest = entries.pop().expect("an entry was just appended");
    atomic_write(&directory.join(PREVIOUS_FILE_NAME), &encode(&entries)?)?;
    atomic_write(&path, &encode(&[latest])?)
}

fn encode(entries: &[ActivityEntry]) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn redact_event(event: ActivityEvent, redactor: &Redactor) -> ActivityEvent {
    let text = |value: String| bounded(&redactor.text(&value), TEXT_LIMIT);
    match event {
        ActivityEvent::JobFailed { code } => ActivityEvent::JobFailed { code: text(code) },
        ActivityEvent::StageFailed { code, detail } => ActivityEvent::StageFailed {
            code: text(code),
            detail: text(detail),
        },
        ActivityEvent::Process {
            label,
            outcome,
            exit_code,
            stdout,
            stderr,
        } => ActivityEvent::Process {
            label: text(label),
            outcome: text(outcome),
            exit_code,
            stdout: redactor.text(&stdout),
            stderr: redactor.text(&stderr),
        },
        ActivityEvent::Download {
            target,
            host,
            status,
            error,
        } => ActivityEvent::Download {
            target: text(target),
            host: text(host),
            status,
            error: text(error),
        },
        other => other,
    }
}

/// The last `limit` bytes of `bytes`, as text.
pub(crate) fn tail_text(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 {
        format!("…{text}")
    } else {
        text
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A short, key-free name for a process: `wineserver -w`, `wine
/// Installer.exe`, or the executable's own name. Arguments that are options
/// (such as the installer's `--key1=`) are never used.
pub(crate) fn process_label(executable: &Path, arguments: &[std::ffi::OsString]) -> String {
    let name = |value: &OsStr| {
        let value = value.to_string_lossy();
        let base = value
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned();
        bounded(&base, 64)
    };
    let program = arguments
        .iter()
        .filter(|argument| {
            let argument = argument.to_string_lossy();
            !argument.starts_with('-') && !argument.ends_with("/wine")
        })
        .map(|argument| name(argument))
        .next();
    let executable_name = name(executable.as_os_str());
    match program {
        Some(program) if executable_name.chars().all(|c| c.is_ascii_digit()) => {
            format!("wine {program}")
        }
        _ if executable_name.chars().all(|c| c.is_ascii_digit()) => {
            let options = arguments
                .iter()
                .map(|argument| argument.to_string_lossy())
                .filter(|argument| matches!(argument.as_ref(), "-k" | "-w"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("wineserver {options}").trim_end().to_owned()
        }
        _ => executable_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn download(host: &str) -> ActivityEntry {
        ActivityEntry {
            at: 1,
            job: Some(3),
            event: ActivityEvent::Download {
                target: "wine".to_owned(),
                host: host.to_owned(),
                status: Some(503),
                error: "status".to_owned(),
            },
            repeat: 1,
        }
    }

    #[test]
    fn live_output_masks_keys_and_the_home_folder() {
        let home = std::env::var("HOME").unwrap_or_default();
        live_output(&format!(
            "installer --key1=ABCDEFGHJKLMNPQRSTVWXYZ234 at {home}/.pd2launcher/game"
        ));
        let shown = recent_lines(RECENT_LINES)
            .into_iter()
            .rfind(|line| line.contains("installer"))
            .unwrap();
        assert!(!shown.contains("ABCDEFGHJKLMNPQRSTVWXYZ234"), "{shown}");
        if home.len() > 1 {
            assert!(!shown.contains(&home), "{shown}");
        }
    }

    #[test]
    fn entries_are_bounded_rotated_and_repeats_coalesce() {
        let fixture = TempDir::new().unwrap();
        let directory = fixture.path().join("diagnostics");
        append(&directory, download("a.example")).unwrap();
        append(&directory, download("a.example")).unwrap();
        let entries = read(&directory);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].repeat, 2);

        let big = "x".repeat(OUTPUT_TAIL);
        for index in 0..200 {
            append(
                &directory,
                ActivityEntry {
                    at: index,
                    job: None,
                    event: ActivityEvent::Process {
                        label: "wine Installer.exe".to_owned(),
                        outcome: "exited".to_owned(),
                        exit_code: Some(1),
                        stdout: big.clone(),
                        stderr: big.clone(),
                    },
                    repeat: 1,
                },
            )
            .unwrap();
        }
        for name in [FILE_NAME, PREVIOUS_FILE_NAME] {
            let metadata = std::fs::metadata(directory.join(name)).unwrap();
            assert!(metadata.len() as usize <= FILE_LIMIT);
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o077, 0);
        }
        let entries = read(&directory);
        assert_eq!(entries.last().unwrap().at, 199);
    }

    #[test]
    fn process_labels_never_carry_option_values() {
        let arguments = [
            "/proc/self/fd/236/wine",
            "/proc/self/fd/235/x/Installer.exe",
            "--key1=SECRET",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            process_label(Path::new("/proc/self/fd/230"), &arguments),
            "wine Installer.exe"
        );
        assert_eq!(
            process_label(Path::new("/proc/self/fd/232"), &["-w".into()]),
            "wineserver -w"
        );
        assert_eq!(
            process_label(Path::new("/usr/bin/true"), &["--key1=SECRET".into()]),
            "true"
        );
    }
}
