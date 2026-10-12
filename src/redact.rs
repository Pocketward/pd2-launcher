//! Redaction for text the Launcher keeps for, or hands to, maintainers.
//!
//! Every text that enters the failure activity log or a saved report passes
//! through [`Redactor::text`]:
//!
//! - CD keys: the in-memory keys (when the process holds them, for example
//!   `D2_KEY`/`D2_LOD_KEY` of an unattended run) are replaced wherever they
//!   appear, in any case. Any key-shaped token — a run of letters, digits and
//!   dashes with exactly 16 or 26 letters and digits that has a digit or no
//!   lowercase letter — is replaced too, as is the value after a key name
//!   (`D2_KEY=…`, `CD key: …`). This errs on the side of hiding: a 16-digit
//!   hex address in a Wine trace is hidden as well.
//! - `$HOME` becomes `~` (also in its Wine `Z:\…` form) and the user name
//!   becomes `<user>`.
//! - URLs lose their credentials, query string and fragment.
//!
//! [`mask_game_identities`] additionally hides account, character and game
//! names in Battle.net and game logs (best effort, see docs/DIAGNOSTICS.md).

use std::{env, path::Path};

pub(crate) const KEY_MASK: &str = "<cd-key>";
const USER_MASK: &str = "<user>";
const NAME_MASK: &str = "<name>";

/// Key names whose value is always a CD key.
const KEY_CONTEXTS: [&str; 10] = [
    "key1",
    "key2",
    "d2_lod_key",
    "d2_key",
    "lod_key",
    "cdkey",
    "cd-key",
    "cd key",
    "product key",
    "product_key",
];

#[derive(Clone, Debug, Default)]
pub(crate) struct Redactor {
    home: Option<String>,
    user: Option<String>,
    /// Upper-case CD keys known to this process, never logged.
    secrets: Vec<String>,
}

impl Redactor {
    pub(crate) fn new(
        home: Option<&Path>,
        user: Option<&str>,
        secrets: impl IntoIterator<Item = String>,
    ) -> Self {
        let home = home
            .and_then(Path::to_str)
            .map(|home| home.trim_end_matches('/').to_owned())
            .filter(|home| home.len() > 1);
        let user = user
            .map(str::to_owned)
            .or_else(|| {
                home.as_deref()
                    .and_then(|home| home.rsplit('/').next())
                    .map(str::to_owned)
            })
            .filter(|user| user.len() >= 2);
        let secrets = secrets
            .into_iter()
            .map(|secret| {
                secret
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .collect::<String>()
                    .to_ascii_uppercase()
            })
            .filter(|secret| secret.len() >= 6)
            .collect();
        Self {
            home,
            user,
            secrets,
        }
    }

    /// This process's home, user name, and any CD keys in its environment.
    /// `.env.local` is never read.
    pub(crate) fn from_environment() -> Self {
        let home = env::var_os("HOME").map(std::path::PathBuf::from);
        let user = env::var("USER")
            .ok()
            .or_else(|| env::var("LOGNAME").ok())
            .filter(|user| !user.is_empty());
        let secrets = ["D2_KEY", "D2_LOD_KEY"]
            .into_iter()
            .filter_map(|name| env::var(name).ok())
            // A dev build's compiled-in keys.
            .chain(
                crate::dev_build::product_keys()
                    .into_iter()
                    .flat_map(|(base, expansion)| [base, expansion])
                    .map(str::to_owned),
            );
        Self::new(home.as_deref(), user.as_deref(), secrets)
    }

    pub(crate) fn text(&self, input: &str) -> String {
        let text = strip_url_secrets(input);
        let text = self.mask_secrets(&text);
        let text = mask_key_contexts(&text);
        let text = mask_key_shaped(&text);
        let text = self.mask_home(&text);
        self.mask_user(&text)
    }

    /// A path for a report: under the home it starts with `~`.
    pub(crate) fn path(&self, path: &Path) -> String {
        self.text(&path.to_string_lossy())
    }

    fn mask_secrets(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.secrets {
            text = replace_ignore_ascii_case(&text, secret, KEY_MASK);
            // The same key typed in groups (`XXXX-XXXX-…`).
            let grouped = secret
                .as_bytes()
                .chunks(4)
                .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                .collect::<Vec<_>>()
                .join("-");
            text = replace_ignore_ascii_case(&text, &grouped, KEY_MASK);
        }
        text
    }

    fn mask_home(&self, text: &str) -> String {
        let Some(home) = &self.home else {
            return text.to_owned();
        };
        let text = text.replace(home.as_str(), "~");
        // Wine names the same folder `Z:\home\user`.
        let windows = format!("Z:{}", home.replace('/', "\\"));
        let text = replace_ignore_ascii_case(&text, &windows, "~");
        replace_ignore_ascii_case(&text, &home.replace('/', "\\"), "~")
    }

    fn mask_user(&self, text: &str) -> String {
        match &self.user {
            Some(user) => replace_ignore_ascii_case(text, user, USER_MASK),
            None => text.to_owned(),
        }
    }
}

fn replace_ignore_ascii_case(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() || needle.len() > text.len() {
        return text.to_owned();
    }
    let lower = text.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut output = String::with_capacity(text.len());
    let mut position = 0;
    while let Some(found) = lower[position..].find(&needle) {
        let start = position + found;
        output.push_str(&text[position..start]);
        output.push_str(replacement);
        position = start + needle.len();
    }
    output.push_str(&text[position..]);
    output
}

/// Remove `user:password@`, `?query` and `#fragment` from every URL.
fn strip_url_secrets(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(found) = rest.find("://") {
        let scheme_start = rest[..found]
            .rfind(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
            })
            .map_or(0, |index| index + 1);
        if scheme_start == found {
            output.push_str(&rest[..found + 3]);
            rest = &rest[found + 3..];
            continue;
        }
        output.push_str(&rest[..found + 3]);
        let after = &rest[found + 3..];
        let end = after
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, '"' | '\'' | '<' | '>' | '`' | '\\')
            })
            .unwrap_or(after.len());
        let url = &after[..end];
        let authority_end = url.find(['/', '?', '#']).unwrap_or(url.len());
        let (authority, path) = url.split_at(authority_end);
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let path = path.split(['?', '#']).next().unwrap_or("");
        output.push_str(authority);
        output.push_str(path);
        rest = &after[end..];
    }
    output.push_str(rest);
    output
}

/// Hide the value after a CD-key name (`D2_KEY=…`, `CD key: …`).
fn mask_key_contexts(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut hidden: Vec<(usize, usize)> = Vec::new();
    for context in KEY_CONTEXTS {
        let mut position = 0;
        while let Some(found) = lower[position..].find(context) {
            let mut index = position + found + context.len();
            position = index;
            let bytes = text.as_bytes();
            // Separator: `=`, `:`, quotes or spaces, at least one of them.
            let separator_start = index;
            while index < bytes.len() && matches!(bytes[index], b'=' | b':' | b' ' | b'"' | b'\'') {
                index += 1;
            }
            if index == separator_start {
                continue;
            }
            let value_start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'-')
            {
                index += 1;
            }
            if index > value_start {
                hidden.push((value_start, index));
            }
        }
    }
    replace_ranges(text, hidden, KEY_MASK)
}

/// Hide every token shaped like a Diablo II (16) or Lord of Destruction (26)
/// CD key.
fn mask_key_shaped(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut hidden = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'-')
        {
            index += 1;
        }
        let mut end = index;
        while end > start && bytes[end - 1] == b'-' {
            end -= 1;
        }
        let token = &bytes[start..end];
        if key_shaped(token) {
            hidden.push((start, end));
        }
    }
    replace_ranges(text, hidden, KEY_MASK)
}

/// 16 or 26 letters and digits: upper case (as keys are printed, grouped by
/// dashes or not), or lower case without dashes and mixing letters and
/// digits (as a key typed in lower case) — never a dashed lower-case name
/// such as `pd2launcher-gecko`.
fn key_shaped(token: &[u8]) -> bool {
    let alphanumeric = token.iter().filter(|byte| **byte != b'-').count();
    if !matches!(alphanumeric, 16 | 26) {
        return false;
    }
    let lower = token
        .iter()
        .filter(|byte| byte.is_ascii_lowercase())
        .count();
    let upper = token
        .iter()
        .filter(|byte| byte.is_ascii_uppercase())
        .count();
    let digits = token.iter().filter(|byte| byte.is_ascii_digit()).count();
    let dashed = token.contains(&b'-');
    (lower == 0 && (upper > 0 || digits > 0))
        || (upper == 0 && !dashed && digits >= 3 && lower >= 3)
}

fn replace_ranges(text: &str, mut ranges: Vec<(usize, usize)>, replacement: &str) -> String {
    if ranges.is_empty() {
        return text.to_owned();
    }
    ranges.sort_unstable();
    let mut output = String::with_capacity(text.len());
    let mut position = 0;
    for (start, end) in ranges {
        if start < position {
            position = position.max(end);
            continue;
        }
        output.push_str(&text[position..start]);
        output.push_str(replacement);
        position = end;
    }
    output.push_str(&text[position..]);
    output
}

/// Words after which Battle.net and game logs name an account, a character,
/// or a game.
const IDENTITY_CONTEXTS: [&str; 12] = [
    "account",
    "acct",
    "character",
    "charname",
    "char",
    "username",
    "user",
    "player",
    "realm",
    "name",
    "game",
    "password",
];

/// Best effort: hide quoted strings and the word after an identity context
/// (`Account: Foo`, `char=Bar`, `Joined game Baz`) in Battle.net and game
/// logs.
pub(crate) fn mask_game_identities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        output.push_str(&mask_line_identities(line));
    }
    output
}

fn mask_line_identities(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut hidden = Vec::new();
    // Quoted strings.
    let mut index = 0;
    while index < bytes.len() {
        if matches!(bytes[index], b'"' | b'\'')
            && let Some(close) = line[index + 1..].find(bytes[index] as char)
        {
            let end = index + 1 + close;
            if end > index + 1 {
                hidden.push((index + 1, end));
            }
            index = end + 1;
            continue;
        }
        index += 1;
    }
    let lower = line.to_ascii_lowercase();
    for context in IDENTITY_CONTEXTS {
        let mut position = 0;
        while let Some(found) = lower[position..].find(context) {
            let start = position + found;
            let mut index = start + context.len();
            position = index;
            // A whole word only.
            if start > 0 && bytes[start - 1].is_ascii_alphabetic() {
                continue;
            }
            while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            let separator_start = index;
            while index < bytes.len() && matches!(bytes[index], b'=' | b':' | b' ' | b'\t') {
                index += 1;
            }
            if index == separator_start {
                continue;
            }
            let value_start = index;
            while index < bytes.len()
                && !bytes[index].is_ascii_whitespace()
                && !matches!(bytes[index], b',' | b';' | b')' | b']' | b'"' | b'\'')
            {
                index += 1;
            }
            if index > value_start {
                hidden.push((value_start, index));
            }
        }
    }
    replace_ranges(line, hidden, NAME_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::new(
            Some(Path::new("/home/alice")),
            Some("alice"),
            ["ABCD2345EFGH6789JKLM234567".to_owned()],
        )
    }

    #[test]
    fn keys_home_user_and_url_secrets_are_hidden() {
        let redactor = redactor();
        let text = redactor.text(
            "key abcd2345efgh6789jklm234567 at /home/alice/x Z:\\home\\alice\\y C:\\users\\alice \
             https://bob:pw@mirror.example/a/b.tar?token=t0p#frag D2_KEY=QWERTY12 \
             classic 2468-BCDE-FGHJ-KMNP other 7Q8W9E0R1T2Y3U4I5O6P7A8S9D typed 7q8w9e0r1t2y3u4i",
        );
        assert!(!text.to_ascii_lowercase().contains("abcd2345"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        assert!(!text.contains("token") && !text.contains("bob") && !text.contains("frag"));
        assert!(text.contains("https://mirror.example/a/b.tar"), "{text}");
        assert!(!text.contains("QWERTY12"), "{text}");
        assert!(!text.contains("2468-BCDE"), "{text}");
        assert!(!text.contains("7Q8W9E0R"), "{text}");
        assert!(!text.contains("7q8w9e0r"), "{text}");
        assert!(text.contains("~/x") && text.contains("~\\y"), "{text}");
        assert!(text.contains("C:\\users\\<user>"), "{text}");
    }

    #[test]
    fn ordinary_words_hashes_and_codes_survive() {
        let redactor = redactor();
        let sha = "0e8c0ea0a9bc50012fd9286d8941736f8a99d43af10413f9062db7773199efc2";
        let text = format!(
            "base-game-key-rejected Diablo II Install Log.html {sha} internationalism 2026-09-28 \
             prefix/.pd2launcher-gecko.sha256 wine-11.11-amd64-wow64 BaseGameWrapper"
        );
        assert_eq!(redactor.text(&text), text);
    }

    #[test]
    fn game_identities_are_masked() {
        let text = mask_game_identities(
            "12:00 Logon account: Hunter2Acct\nJoined game Baal-Run-12 as character=Sorc\nsaid \"hi there\"\n",
        );
        assert!(!text.contains("Hunter2Acct"), "{text}");
        assert!(!text.contains("Baal-Run-12"), "{text}");
        assert!(!text.contains("Sorc"), "{text}");
        assert!(!text.contains("hi there"), "{text}");
    }
}
