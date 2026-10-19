//! What a start behind the proxy logs about a deny list it cannot apply
//! (`Args::proxy`, `docs/api.md` "Behind the farm's proxy"). Alone in its test binary: a log
//! event is seen by a subscriber set for one thread only if no other thread has already made
//! the callsite uninteresting.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser as _;
use kbf_server::Args;

/// A `MakeWriter` into a shared buffer, standing for the log.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
        .expect("UTF-8")
    }
}

fn file(content: &[u8], mode: u32) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-proxy-auth-warns");
    std::fs::create_dir_all(&dir).expect("a directory");
    let path = dir.join(format!(
        "file-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, content).expect("write the file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path.to_str().expect("UTF-8").to_owned()
}

/// What `Args::proxy` logs for a deny list of `entries`, with or without the proxy flag.
fn logged(entries: &str, behind: bool) -> String {
    let list = file(entries.as_bytes(), 0o600);
    let secret = file(&[0x5a; 32], 0o600);
    let mut argv = vec![
        "kbf-server",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
    ];
    let deny = format!("--worker-deny-list={list}");
    argv.push(&deny);
    let proxy = format!("--proxy-auth-file={secret}");
    if behind {
        argv.push(&proxy);
    }
    let args = Args::try_parse_from(argv).expect("flags");
    let log = Log::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        args.proxy().expect("a usable proxy");
    });
    log.text()
}

const SERIAL: &str = "serial 0a1b2c\n";
const SPKI: &str = "spki-sha256 0000000000000000000000000000000000000000000000000000000000000000\n";
const NODE: &str = "node mac-07\n";

/// Catches: a deny list whose serial or key entries can never match, and nothing said.
///
/// Mutants seen red: the warning dropped; the count left out of its condition for `serial`
/// entries; for `spki-sha256` entries.
#[test]
fn serial_and_spki_entries_behind_the_proxy_are_warned_about() {
    for (entries, count) in [
        (SERIAL, "entries=1"),
        (SPKI, "entries=1"),
        (&format!("{NODE}{SERIAL}{SPKI}") as &str, "entries=2"),
    ] {
        let text = logged(entries, true);
        assert!(text.contains("WARN"), "{entries}: {text}");
        assert!(
            text.contains("serial or spki-sha256 entries") && text.contains(count),
            "{entries}: {text}"
        );
        assert!(text.contains("--proxy-auth-file"), "{text}");
    }
}

/// Catches: a warning for a list the proxy can apply (node entries only), and one without
/// the flag, where serial and key entries match the certificate.
///
/// Mutants seen red: the warning on node entries; the warning without the proxy flag.
#[test]
fn a_list_the_proxy_can_apply_is_not_warned_about() {
    assert!(!logged(NODE, true).contains("WARN"));
    assert!(!logged("# none\n", true).contains("WARN"));
    assert!(!logged(&format!("{NODE}{SERIAL}"), false).contains("WARN"));
}
