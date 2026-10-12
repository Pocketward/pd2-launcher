# Problem reports

A problem report is one file that says which setup step failed, why, on what
system, and what the failing program printed. It never includes your CD keys.

## Saving one

- **Diagnostics → Report a problem → Save report**, or **Save report** on any
  failure card next to *Try again*.
- `pd2-launcher --save-report` does the same from a terminal: the window opens,
  saves the report, prints its path and closes (exit status 8 if it could not be
  saved).

The report is saved as
`~/.pd2launcher/logs/reports/pd2-launcher-report-<UTC>.tar.xz` (under
`PD2_LAUNCHER_HOME` when that is set), readable only by you; the newest 10 are
kept. The Launcher opens the reports folder, and **Copy path** copies the
file's location. Attach the file when you open an issue.

## What it contains

- `summary.txt`: the Launcher's version, the system, session and GPU, each
  setup step's state, and the most recent failure with its code and reason.
- `report.json`: the same in detail: step 1's checks of the computer, a
  read-only check of the installation, the setup steps, the recent jobs with
  their failure codes, and the settings (download sources by host name only).
- `logs/`: the output of programs that failed, the Launcher's own logs,
  Blizzard's installer logs, Project Diablo 2's update and game logs, and the
  Diablo II registry keys (only their path, version, language, video and realm
  values). Each file keeps at most its last 256 KiB.

## What is removed

Every text in the report passes through `src/redact.rs` first:

- CD keys, and every token shaped like one, become `<cd-key>`.
- Your home folder becomes `~` and your user name `<user>`.
- URLs lose credentials, query string and fragment; download sources are
  reduced to host names.
- Account, character and game names in Battle.net and game logs become
  `<name>` where the log labels them.
- Program arguments are never recorded.
