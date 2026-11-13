//! The app's log file, with secrets taken out.
//!
//! Lines go to `logs/clio-notes.log` in the app's data folder, next to the
//! settings. The file is rotated at 2 MB (one old file is kept as
//! `clio-notes.1.log`).
//!
//! What is logged is chosen so that meeting content never reaches the log:
//! only counts, timings, device and model names, and error messages. Every
//! line also passes through [`redact`], which removes anything shaped like
//! an API key or a bearer token, and is cut to [`MAX_LINE`] characters.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The longest line written, in characters.
const MAX_LINE: usize = 600;
const MAX_BYTES: u64 = 2 * 1024 * 1024;

struct FileLogger {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl FileLogger {
    fn open(path: &Path) -> Option<File> {
        OpenOptions::new().create(true).append(true).open(path).ok()
    }

    fn rotate_if_full(&self, file: &mut Option<File>) {
        let full = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() >= MAX_BYTES);
        if full {
            *file = None;
            let _ = std::fs::rename(&self.path, self.path.with_file_name("clio-notes.1.log"));
            *file = Self::open(&self.path);
        }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
            // Dependencies log at info level about every HTTP connection.
            || metadata.target().starts_with("clio")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let target = record.target();
        let quiet = ["reqwest", "hyper", "rustls", "tao", "wry", "tracing"];
        if record.level() > log::Level::Warn && quiet.iter().any(|q| target.starts_with(q)) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}\n",
            timestamp(),
            record.level(),
            target,
            redact(&record.args().to_string())
        );
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        self.rotate_if_full(&mut file);
        if let Some(f) = file.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = f.flush();
        }
    }
}

/// The folder holding the log files.
pub fn folder() -> Option<PathBuf> {
    storage::meetings_root()
        .ok()
        .and_then(|root| root.parent().map(|p| p.join("logs")))
}

/// Starts logging to the file and records panics there too.
pub fn init() {
    let Some(dir) = folder() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("clio-notes.log");
    let logger = FileLogger {
        file: Mutex::new(FileLogger::open(&path)),
        path,
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(target: "clio::panic", "{info}");
        log::logger().flush();
        default_hook(info);
    }));
    log::info!(
        target: "clio",
        "Clio Notes {} started",
        env!("CARGO_PKG_VERSION")
    );
}

fn timestamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    pipeline::final_pass::local_time(ms)
}

/// Removes secrets from a log line and cuts it to [`MAX_LINE`] characters.
///
/// Masked: the value after `Bearer`, `api-key`/`x-api-key`/`authorization`
/// and `key=`/`token=`, words starting with a known key prefix (`sk-`,
/// `sk-ant-`, `sk-or-`, `gsk_`, `AIza`, `xai-`), and any other unbroken run of
/// 32 or more letters, digits, `-` and `_`, which is how keys look.
pub fn redact(text: &str) -> String {
    const PREFIXES: [&str; 6] = ["sk-", "gsk_", "AIza", "xai-", "pk-", "rk-"];
    const LABELS: [&str; 7] = [
        "bearer",
        "api-key",
        "x-api-key",
        "api_key",
        "authorization",
        "key=",
        "token=",
    ];
    let is_key_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';

    let mut out = String::with_capacity(text.len());
    let mut mask_next = false;
    let mut rest = text;
    while !rest.is_empty() {
        // The next run of key characters, and what comes before it.
        let start = rest.find(is_key_char).unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| !is_key_char(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        let after = &rest[end..];

        let looks_like_key =
            word.len() >= 32 || (word.len() >= 12 && PREFIXES.iter().any(|p| word.starts_with(p)));
        if mask_next && word.len() >= 8 || looks_like_key {
            out.push_str("[redacted]");
            mask_next = false;
        } else {
            out.push_str(word);
            let lower = word.to_ascii_lowercase();
            let label = format!("{lower}{}", after.chars().next().unwrap_or(' '));
            // A label followed by ":", "=" or a space puts a secret next.
            mask_next = LABELS
                .iter()
                .any(|l| label == *l || lower == l.trim_end_matches('='));
        }
        rest = after;
    }
    if out.chars().count() > MAX_LINE {
        out = out.chars().take(MAX_LINE).collect::<String>() + "…";
    }
    out.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn keys_and_tokens_are_masked() {
        let cases = [
            (
                "Authorization: Bearer abcdefghijkl123456",
                "Authorization: Bearer [redacted]",
            ),
            ("key sk-ant-api03-abcdefgh1234", "key [redacted]"),
            ("got sk-or-v1-0123456789abc from", "got [redacted] from"),
            ("x-api-key: shortbutsecret1", "x-api-key: [redacted]"),
            ("url?key=AIzaSyA1234567890&x=1", "url?key=[redacted]&x=1"),
            (
                "token abcdefghijABCDEFGHIJ0123456789abcd here",
                "token [redacted] here",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn ordinary_text_is_kept() {
        let line = "Meeting started: mic \"Headset (USB-C)\", model gpt-transcribe, 12 lines";
        assert_eq!(redact(line), line);
        assert_eq!(
            redact("Could not reach api.openai.com. (dns error)"),
            "Could not reach api.openai.com. (dns error)"
        );
    }

    #[test]
    fn lines_are_cut_and_kept_on_one_line() {
        let long = "word ".repeat(400);
        let out = redact(&long);
        assert!(out.chars().count() <= 601);
        assert!(!redact("a\nb").contains('\n'));
    }
}
