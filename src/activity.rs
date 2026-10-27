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
        /// `exited`, `timed-out`, `stalled` (a download that stopped
        /// changing), `spawn-failed`, or `wait-failed`.
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stdout: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stderr: String,
    },
    /// A file of the payload Blizzard's downloader left that did not verify:
    /// the payload folder, the file in it and the check it failed (`missing`,
    /// `partial`, `unexpected`, `not-a-file`, `unreadable`, `size`,
    /// `not-a-program`, `sha256`), with the size found for `size`.
    PayloadRejected {
        payload: String,
        file: String,
        check: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
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

/// [`redacted`] for a line of the game's own logs: a row of a crash report's
/// memory dump keeps its hex words ([`Redactor::game_log_text`]).
pub(crate) fn redacted_game_log(text: &str) -> String {
    live_redactor().game_log_text(text)
}

fn live_redactor() -> &'static Redactor {
    static LIVE: std::sync::OnceLock<Redactor> = std::sync::OnceLock::new();
    LIVE.get_or_init(Redactor::from_environment)
}

/// One line a running program wrote, for the progress log as it happens.
/// A line of Wine's Winsock channel (Detailed logging) goes to standard
/// error alone, and only when [`winsock_line`] keeps it. The line's class
/// comes first: Wine writes a Winsock line for each call, and the many it
/// drops need no redaction.
pub(crate) fn live_output(line: &str, detailed: bool) {
    let raw = line.trim_end();
    if raw.trim().is_empty() {
        return;
    }
    if NETWORK_LEFT_OUT.load(std::sync::atomic::Ordering::Relaxed) {
        network_lines(|lines, millis| lines.second_passed(millis));
    }
    match winsock_line(raw) {
        WinsockLine::Other => {}
        WinsockLine::Dropped => return,
        WinsockLine::Kept(event) => {
            let event = redacted(&event);
            network_lines(|lines, millis| lines.event(millis, &event));
            return;
        }
    }
    let line = redacted(raw);
    let line = line.as_str();
    // Also on standard error, where DroidDeck's session log keeps it. With
    // Detailed logging longer, so a .NET stack trace keeps its lines.
    let limit = output_line_limit(detailed);
    eprintln!("pd2-launcher: | {}", bounded(line, limit));
    if let Ok(mut recent) = RECENT.lock() {
        if recent.len() == RECENT_LINES {
            recent.pop_front();
        }
        recent.push_back(format!("  {}", bounded(line, 300)));
        RECENT_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How long a program's output line on standard error may be: with Detailed
/// logging longer, so a .NET stack trace keeps its lines.
const fn output_line_limit(detailed: bool) -> usize {
    if detailed { 1000 } else { 300 }
}

/// What [`live_output`] does with a line of a program's output.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum WinsockLine {
    /// Not Wine's Winsock channel: shown as it is.
    Other,
    /// A Winsock line of no use (a send, a receive, a wait): left out.
    Dropped,
    /// A Winsock line kept: Wine's process and thread, the function and its
    /// words, without Wine's own clock.
    Kept(String),
}

/// The Winsock functions whose lines Detailed logging keeps: connects and
/// closes, with the address, and name lookups. Errors and warnings are
/// kept from every function.
const WINSOCK_KEPT: [&str; 10] = [
    "connect",
    "WSAConnect",
    "WS2_ConnectEx",
    "closesocket",
    "shutdown",
    "WSASocketW",
    "getaddrinfo",
    "GetAddrInfoW",
    "GetAddrInfoExW",
    "gethostbyname",
];

/// `line` as a line of Wine's Winsock debug channel
/// (`[time:][pid:]tid:class:winsock:function words`), which Detailed logging
/// turns on ([`crate::wine_environment::DETAILED_WINEDEBUG`]): Wine writes one
/// for each call, so most go.
pub(crate) fn winsock_line(line: &str) -> WinsockLine {
    let Some(at) = line.find(":winsock:") else {
        return WinsockLine::Other;
    };
    let mut head = line[..at].split(':').collect::<Vec<_>>();
    let class = head.pop().unwrap_or_default();
    let rest = &line[at + ":winsock:".len()..];
    let (function, words) = rest.split_once(' ').unwrap_or((rest, ""));
    let error = matches!(class, "err" | "warn");
    if !error && !WINSOCK_KEPT.contains(&function) {
        return WinsockLine::Dropped;
    }
    // Wine's own clock (`123.456`) is time since boot: the engine gives its own.
    let ids = head
        .into_iter()
        .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .collect::<Vec<_>>()
        .join(":");
    let mut kept = String::new();
    if !ids.is_empty() {
        kept.push_str(&ids);
        kept.push(' ');
    }
    if error {
        kept.push_str(class);
        kept.push(' ');
    }
    kept.push_str(function);
    if !words.trim().is_empty() {
        kept.push(' ');
        kept.push_str(words.trim());
    }
    WinsockLine::Kept(bounded(&kept, 300))
}

/// At most this many network lines a second; the rest are counted.
const NETWORK_LINES_PER_SECOND: u32 = 20;

/// The network lines of one wall-clock second: those written, and those left
/// out, whose count goes out once that second is over (at the next line of
/// any program, or when a program's output ends) with the second it
/// belongs to.
#[derive(Debug, Default)]
pub(crate) struct NetworkLines {
    second: u64,
    lines: u32,
    left_out: u32,
}

impl NetworkLines {
    /// The lines to write for a kept Winsock `event` at `millis` (UTC, ms
    /// since 1970).
    pub(crate) fn event(&mut self, millis: u64, event: &str) -> Vec<String> {
        let mut out = self.second_passed(millis);
        let second = millis / 1000;
        if self.lines > 0 && second == self.second {
            if self.lines >= NETWORK_LINES_PER_SECOND {
                self.left_out += 1;
                return out;
            }
            self.lines += 1;
        } else {
            self.second = second;
            self.lines = 1;
        }
        out.push(format!(
            "pd2-launcher: network {} {event}",
            crate::report::utc_time_millis(millis)
        ));
        out
    }

    /// The count of the lines left out, once their second is over at
    /// `millis`.
    pub(crate) fn second_passed(&mut self, millis: u64) -> Vec<String> {
        if millis / 1000 == self.second {
            return Vec::new();
        }
        self.flush().into_iter().collect()
    }

    /// The count of the lines left out so far, if any.
    pub(crate) fn flush(&mut self) -> Option<String> {
        if self.left_out == 0 {
            return None;
        }
        let line = format!(
            "pd2-launcher: network: {} more lines in the second from {} were left out",
            self.left_out,
            crate::report::utc_time_millis(self.second * 1000)
        );
        self.left_out = 0;
        Some(line)
    }

    fn left_out(&self) -> bool {
        self.left_out > 0
    }
}

static NETWORK_LINES: std::sync::Mutex<NetworkLines> = std::sync::Mutex::new(NetworkLines {
    second: 0,
    lines: 0,
    left_out: 0,
});
/// Whether [`NETWORK_LINES`] has a count to write: each output line then
/// looks whether its second is over.
static NETWORK_LEFT_OUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `step` on the network lines at the time now; its lines go to standard
/// error.
fn network_lines(step: impl FnOnce(&mut NetworkLines, u64) -> Vec<String>) {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    let Ok(mut lines) = NETWORK_LINES.lock() else {
        return;
    };
    for line in step(&mut lines, millis) {
        eprintln!("{line}");
    }
    NETWORK_LEFT_OUT.store(lines.left_out(), std::sync::atomic::Ordering::Relaxed);
}

/// A program's output ended (it stopped): the count of network lines left
/// out so far.
pub(crate) fn output_ended() {
    if NETWORK_LEFT_OUT.load(std::sync::atomic::Ordering::Relaxed) {
        network_lines(|lines, _| lines.flush().into_iter().collect());
    }
}

/// What the capture that a program's result keeps ([`crate::host_execution`])
/// does with a line of its output.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CaptureLine {
    Keep,
    /// Wine's `+loaddll` lines and the Winsock lines that [`winsock_line`]
    /// drops: the capture of a failed program then holds its error.
    Drop,
    /// A Winsock line that [`winsock_line`] keeps: at most
    /// [`NETWORK_LINES_PER_SECOND`] a second ([`CaptureNetworkLines`]).
    Network,
}

/// The capture's [`CaptureLine`] for `line`. `engine.log` has every line
/// but the dropped Winsock lines.
pub(crate) fn capture_line(line: &[u8]) -> CaptureLine {
    let line = String::from_utf8_lossy(line);
    if line.contains(":trace:loaddll:") {
        return CaptureLine::Drop;
    }
    match winsock_line(&line) {
        WinsockLine::Other => CaptureLine::Keep,
        WinsockLine::Dropped => CaptureLine::Drop,
        WinsockLine::Kept(_) => CaptureLine::Network,
    }
}

/// The network lines one capture takes in a second of the wall clock.
#[derive(Debug, Default)]
pub(crate) struct CaptureNetworkLines {
    second: u64,
    lines: u32,
}

impl CaptureNetworkLines {
    /// Whether the capture takes a network line at `second` (seconds since
    /// 1970).
    pub(crate) fn take(&mut self, second: u64) -> bool {
        if second != self.second {
            self.second = second;
            self.lines = 0;
        }
        self.lines += 1;
        self.lines <= NETWORK_LINES_PER_SECOND
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
        ActivityEvent::PayloadRejected {
            payload,
            file,
            check,
            size,
        } => push(format!(
            "{payload}: {file} failed the {check} check{}",
            size.map_or_else(String::new, |size| format!(" ({size} bytes)"))
        )),
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
/// A downloaded payload's file that did not verify ([`ActivityEvent::PayloadRejected`]).
pub(crate) fn record_payload_problem(payload: &str, file: &str, check: &str, size: Option<u64>) {
    eprintln!(
        "pd2-launcher: payload {}: {} failed the {check} check",
        redacted(payload),
        redacted(file)
    );
    record(ActivityEvent::PayloadRejected {
        payload: payload.to_owned(),
        file: file.to_owned(),
        check: check.to_owned(),
        size,
    });
}

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

/// Classify a transport error without its message (which names the URL). A
/// connection that failed says why when its causes tell: `connection-blocked`
/// (the system refused this app the network), `dns-failed`,
/// `connection-refused`, `network-unreachable`; else `connection-failed`.
/// A phone's per-app block mostly drops packets instead of refusing them, so
/// it shows here as `timed-out` or `dns-failed`: the app's network check
/// (`files/logs/network.log` on Android) is what tells a block.
pub(crate) fn transport_error_kind(error: &zed_reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed-out"
    } else if error.is_connect() {
        connect_failure_kind(error)
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

/// Why a connection failed, from the errors that caused it: their I/O kinds,
/// and the resolver's own word for a failed lookup (its messages name no URL).
fn connect_failure_kind(error: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut source = error.source();
    let (mut blocked, mut lookup, mut refused, mut unreachable) = (false, false, false, false);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::PermissionDenied => blocked = true,
                std::io::ErrorKind::ConnectionRefused => refused = true,
                std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::HostUnreachable => {
                    unreachable = true;
                }
                _ => {}
            }
        }
        lookup |= cause.to_string().contains("dns error");
        source = cause.source();
    }
    // A block first; a failed lookup over the I/O kind its error carries.
    match (blocked, lookup, refused, unreachable) {
        (true, ..) => "connection-blocked",
        (_, true, ..) => "dns-failed",
        (_, _, true, _) => "connection-refused",
        (_, _, _, true) => "network-unreachable",
        _ => "connection-failed",
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
        ActivityEvent::PayloadRejected {
            payload,
            file,
            check,
            size,
        } => ActivityEvent::PayloadRejected {
            payload: text(payload),
            file: text(file),
            check: text(check),
            size,
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

    /// An error caused by `causes`, innermost last, as a connector reports it.
    #[derive(Debug)]
    struct Chain(String, Option<Box<dyn std::error::Error + 'static>>);

    impl std::fmt::Display for Chain {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl std::error::Error for Chain {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref()
        }
    }

    fn failed_by(cause: Box<dyn std::error::Error + 'static>) -> Chain {
        Chain(
            "error sending request".to_owned(),
            Some(Box::new(Chain(
                "client error (Connect)".to_owned(),
                Some(cause),
            ))),
        )
    }

    #[test]
    fn a_failed_connection_says_why_when_its_causes_tell() {
        let io = |kind| -> Box<dyn std::error::Error> { Box::new(std::io::Error::from(kind)) };
        assert_eq!(
            connect_failure_kind(&failed_by(io(std::io::ErrorKind::PermissionDenied))),
            "connection-blocked"
        );
        assert_eq!(
            connect_failure_kind(&failed_by(io(std::io::ErrorKind::ConnectionRefused))),
            "connection-refused"
        );
        assert_eq!(
            connect_failure_kind(&failed_by(io(std::io::ErrorKind::NetworkUnreachable))),
            "network-unreachable"
        );
        let lookup = Box::new(Chain(
            "dns error".to_owned(),
            Some(Box::new(std::io::Error::other(
                "failed to lookup address information",
            ))),
        ));
        assert_eq!(connect_failure_kind(&failed_by(lookup)), "dns-failed");
        assert_eq!(
            connect_failure_kind(&failed_by(io(std::io::ErrorKind::HostUnreachable))),
            "network-unreachable"
        );
        // A lookup that failed with a refused query is still a failed lookup.
        let refused_lookup = Box::new(Chain(
            "dns error".to_owned(),
            Some(io(std::io::ErrorKind::ConnectionRefused)),
        ));
        assert_eq!(
            connect_failure_kind(&failed_by(refused_lookup)),
            "dns-failed"
        );
        // A block wins over the failed lookup it caused.
        let blocked_lookup = Box::new(Chain(
            "dns error".to_owned(),
            Some(io(std::io::ErrorKind::PermissionDenied)),
        ));
        assert_eq!(
            connect_failure_kind(&failed_by(blocked_lookup)),
            "connection-blocked"
        );
        assert_eq!(
            connect_failure_kind(&failed_by(io(std::io::ErrorKind::Other))),
            "connection-failed"
        );
    }
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
    fn winsock_lines_keep_connects_closes_lookups_and_errors_only() {
        assert_eq!(
            winsock_line(
                "123.456:0020:0024:trace:winsock:connect socket 0x1a4, addr { family AF_INET, \
                 address 35.225.107.249, port 6112 }, len 16"
            ),
            WinsockLine::Kept(
                "0020:0024 connect socket 0x1a4, addr { family AF_INET, address 35.225.107.249, \
                 port 6112 }, len 16"
                    .to_owned()
            )
        );
        assert_eq!(
            winsock_line("0024:trace:winsock:closesocket 0x1a4"),
            WinsockLine::Kept("0024 closesocket 0x1a4".to_owned())
        );
        assert_eq!(
            winsock_line("0020:0024:warn:winsock:WS2_recv_base unsupported flags 0x10"),
            WinsockLine::Kept("0020:0024 warn WS2_recv_base unsupported flags 0x10".to_owned())
        );
        assert_eq!(
            winsock_line(
                "123.456:0020:0024:trace:winsock:WS2_recv_base socket 0x1a4, buffers 0x22f000, \
                 buffer_count 1, flags 0"
            ),
            WinsockLine::Dropped
        );
        assert_eq!(
            winsock_line("0020:0024:trace:winsock:select read 0x22f100, timeout 0x0"),
            WinsockLine::Dropped
        );
        assert_eq!(
            winsock_line("0020:0024:trace:seh:dispatch_exception code=c0000005"),
            WinsockLine::Other
        );
        assert_eq!(winsock_line("[BOX64] Using Box64"), WinsockLine::Other);
    }

    #[test]
    fn network_lines_are_limited_each_second_and_the_count_names_its_second() {
        let start = 1_792_886_400_000;
        let mut lines = NetworkLines::default();
        let written = (0..25)
            .flat_map(|index| lines.event(start + index, "connect x"))
            .collect::<Vec<_>>();
        assert_eq!(written.len(), 20, "{written:?}");
        // A minute later, the count of that second comes first, with its time.
        let later = lines.event(start + 61_000, "closesocket 0x1a4");
        assert_eq!(
            later,
            [
                format!(
                    "pd2-launcher: network: 5 more lines in the second from {} were left out",
                    crate::report::utc_time_millis(start)
                ),
                format!(
                    "pd2-launcher: network {} closesocket 0x1a4",
                    crate::report::utc_time_millis(start + 61_000)
                ),
            ]
        );
        // A count with no network line after it: when its second is over (at
        // another program line), or when a program's output ends.
        for index in 1..22 {
            lines.event(start + 61_000 + index, "connect y");
        }
        assert!(lines.second_passed(start + 61_900).is_empty());
        let over = lines.second_passed(start + 62_000);
        assert_eq!(over.len(), 1);
        assert!(
            over[0].contains(": 2 more lines in the second from"),
            "{over:?}"
        );
        assert_eq!(lines.flush(), None);
        lines.event(start + 63_000, "connect z");
        for index in 1..21 {
            lines.event(start + 63_000 + index, "connect z");
        }
        assert!(
            lines
                .flush()
                .is_some_and(|line| line.contains(": 1 more lines"))
        );
    }

    #[test]
    fn the_capture_keeps_all_but_wine_noise_and_network_lines_twenty_a_second() {
        assert_eq!(
            capture_line(b"0020:0024:trace:loaddll:build_module Loaded L\"ntdll.dll\": builtin"),
            CaptureLine::Drop
        );
        assert_eq!(
            capture_line(b"0020:0024:trace:winsock:select read 0x22f100"),
            CaptureLine::Drop
        );
        assert_eq!(
            capture_line(b"0020:0024:trace:winsock:connect socket 0x1a4"),
            CaptureLine::Network
        );
        assert_eq!(
            capture_line(b"0020:0024:warn:winsock:WS2_recv_base unsupported flags"),
            CaptureLine::Network
        );
        assert_eq!(
            capture_line(b"err:module:import_dll Library X.dll not found"),
            CaptureLine::Keep
        );
        assert_eq!(capture_line(b""), CaptureLine::Keep);
        let mut network = CaptureNetworkLines::default();
        let taken = (0..25).filter(|_| network.take(1_792_886_400)).count();
        assert_eq!(taken, 20);
        assert!(network.take(1_792_886_401));
    }

    #[test]
    fn detailed_logging_shows_longer_output_lines() {
        assert_eq!(output_line_limit(false), 300);
        assert_eq!(output_line_limit(true), 1000);
        let line = "y".repeat(900);
        assert_eq!(bounded(&line, output_line_limit(true)), line);
        assert!(bounded(&line, output_line_limit(false)).len() < line.len());
    }

    #[test]
    fn live_output_masks_keys_and_the_home_folder() {
        let home = std::env::var("HOME").unwrap_or_default();
        live_output(
            &format!("installer --key1=ABCDEFGHJKLMNPQRSTVWXYZ234 at {home}/.pd2launcher/game"),
            false,
        );
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
