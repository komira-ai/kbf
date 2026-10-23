//! The slot profile and the GPU, on every platform: the profile a pool user's lease runs
//! under never denies what Metal needs. Two denies take the GPU away from a sandboxed
//! process (seen on a hosted macOS runner, whose GPU is paravirtual):
//! `(deny iokit-open)` leaves no Metal device, and a `mach-lookup` deny of
//! `com.apple.MTLCompilerService` leaves the device and breaks pipeline creation.
//! `tests/slot_gpu.rs` runs a compute kernel as a pool user on a Mac, under the real
//! profile and under each of these denies; this file needs no Mac.
//!
//! [`breaks_the_gpu`] reads a profile as whole forms (a deny may span lines, and may
//! carry `(with ...)` modifiers) and says whether one is one of those denies,
//! or a broader one that includes them: a blanket `(deny default)`, `(deny iokit*)`, a
//! `mach-lookup` deny with no filter, one that names the service or a prefix of its name,
//! or one by regular expression or `require-not` (which are not evaluated, so never allowed).

use std::path::{Path, PathBuf};

use kbf_driver_native::network::{
    Isolation, NO_DEVICE_RULES, Network, SANDBOX_EXEC, SLOT_NO_NETWORK_PROFILE, SLOT_PROFILE,
    WeakenedProfile, slot_profile, slot_write_rules,
};

/// The Mach service the Metal compiler runs in.
const COMPILER_SERVICE: &str = "com.apple.MTLCompilerService";

/// The two denies that took the GPU away, as the profile text spells them.
const ADDED: [&str; 2] = [
    "(deny iokit-open)",
    "(deny mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
];

/// One element of a profile form.
#[derive(Debug)]
enum Node {
    Atom(String),
    /// A quoted string, `"..."`.
    Text(String),
    /// A regular expression literal, `#"..."`.
    Regex,
    List(Vec<Node>),
}

/// The text of each top-level form of `profile` (balanced parentheses; strings and
/// `;` comments skipped, so a parenthesis inside either does not count) with its parse.
fn forms(profile: &str) -> Vec<(&str, Node)> {
    let bytes = profile.as_bytes();
    let mut at = 0;
    let mut out = Vec::new();
    while at < bytes.len() {
        match bytes[at] {
            b';' => at = skip_comment(bytes, at),
            b'(' => {
                let start = at;
                let node = parse_list(bytes, &mut at);
                out.push((&profile[start..at], node));
            }
            _ => at += 1,
        }
    }
    out
}

fn skip_comment(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && bytes[at] != b'\n' {
        at += 1;
    }
    at
}

/// The string whose opening quote is at `bytes[at]`, and the index after its closing one.
fn parse_string(bytes: &[u8], mut at: usize) -> (String, usize) {
    let mut text = Vec::new();
    at += 1;
    while at < bytes.len() && bytes[at] != b'"' {
        if bytes[at] == b'\\' && at + 1 < bytes.len() {
            at += 1;
        }
        text.push(bytes[at]);
        at += 1;
    }
    (
        String::from_utf8_lossy(&text).into_owned(),
        (at + 1).min(bytes.len()),
    )
}

/// The list whose `(` is at `bytes[*at]`; `*at` ends after its `)` (or at the end of the
/// text for an unclosed one).
fn parse_list(bytes: &[u8], at: &mut usize) -> Node {
    let mut items = Vec::new();
    *at += 1;
    while *at < bytes.len() {
        match bytes[*at] {
            b')' => {
                *at += 1;
                break;
            }
            b'(' => items.push(parse_list(bytes, at)),
            b';' => *at = skip_comment(bytes, *at),
            b'"' => {
                let (text, next) = parse_string(bytes, *at);
                items.push(Node::Text(text));
                *at = next;
            }
            b'#' if bytes.get(*at + 1) == Some(&b'"') => {
                let (_, next) = parse_string(bytes, *at + 1);
                items.push(Node::Regex);
                *at = next;
            }
            c if c.is_ascii_whitespace() => *at += 1,
            _ => {
                let start = *at;
                while *at < bytes.len()
                    && !bytes[*at].is_ascii_whitespace()
                    && !matches!(bytes[*at], b'(' | b')' | b'"' | b';')
                {
                    *at += 1;
                }
                items.push(Node::Atom(
                    String::from_utf8_lossy(&bytes[start..*at]).into_owned(),
                ));
            }
        }
    }
    Node::List(items)
}

/// Whether a deny of `mach-lookup` with these filters includes [`COMPILER_SERVICE`]. No
/// filter is every service; a regular expression is taken to include it, and so is a
/// `require-not`, which denies everything but what it names.
fn denies_service(filters: &[&Node]) -> bool {
    fn includes(node: &Node) -> bool {
        let Node::List(items) = node else {
            return false;
        };
        let name = |at: usize| match items.get(at) {
            Some(Node::Text(text)) => Some(text.as_str()),
            _ => None,
        };
        match items.first() {
            Some(Node::Atom(head)) if head == "require-not" => true,
            Some(Node::Atom(head)) if head.contains("regex") => true,
            Some(Node::Atom(head)) if head == "global-name-prefix" => {
                name(1).is_some_and(|prefix| COMPILER_SERVICE.starts_with(prefix))
            }
            Some(Node::Atom(head)) if head == "global-name" => name(1) == Some(COMPILER_SERVICE),
            _ => items.iter().any(includes),
        }
    }
    filters.is_empty() || filters.iter().any(|f| includes(f))
}

/// Whether `form` denies something Metal needs: one of the operations listed in the
/// match below, alone or among several in one deny. The operations follow `deny`, after
/// any `(with ...)` modifier and before the filters, which may be on later lines. This is
/// a list of known denies, not a proof that Metal works; `tests/slot_gpu.rs` is that
/// proof, on a Mac.
fn form_breaks_the_gpu(form: &Node) -> bool {
    let Node::List(items) = form else {
        return false;
    };
    if !matches!(items.first(), Some(Node::Atom(head)) if head == "deny") {
        return false;
    }
    let is_with = |n: &Node| matches!(n, Node::List(l) if matches!(l.first(), Some(Node::Atom(a)) if a == "with"));
    let operations = items[1..].iter().filter_map(|n| match n {
        Node::Atom(op) => Some(op.as_str()),
        _ => None,
    });
    let filters: Vec<&Node> = items[1..]
        .iter()
        .filter(|n| matches!(n, Node::List(_)) && !is_with(n))
        .collect();
    operations.into_iter().any(|operation| match operation {
        "default"
        | "iokit*"
        | "iokit-open"
        | "iokit-get-properties"
        | "mach*"
        | "ipc-posix-shm*"
        | "ipc-posix-shm-read*"
        | "ipc-posix-shm-read-data" => true,
        "mach-lookup" => denies_service(&filters),
        _ => false,
    })
}

/// Whether `profile` holds a form that denies something Metal needs.
fn breaks_the_gpu(profile: &str) -> bool {
    !breaking(profile).is_empty()
}

/// The profile a slot user's lease runs under for `network`, as the driver generates
/// it (the text after `-p` of the sandbox command line), from `base`, the profile text
/// before the device and write rules are added.
fn generated(base: &str) -> String {
    let rules = slot_write_rules(Path::new("/h/slot"), Path::new("/v/f/xx/yyyy")).expect("rules");
    let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
    let (_, args) = sandbox.wrap_in(base, Path::new("/lease"), &rules, "p".into(), &[]);
    let at = args.iter().position(|a| a == "-p").expect("-p");
    args[at + 1].clone()
}

/// The forms of `profile` that deny something Metal needs, as written.
fn breaking(profile: &str) -> Vec<&str> {
    forms(profile)
        .into_iter()
        .filter(|(_, node)| form_breaks_the_gpu(node))
        .map(|(text, _)| text)
        .collect()
}

/// Catches a slot profile, with or without the network, that denies what a GPU needs:
/// the generated profile of each network setting has no line that is, or includes,
/// either of the denies in `ADDED`; it still starts from `(allow default)`, which is what leaves the
/// Metal services open; and the device rules appended to every profile, which name
/// other services, are not taken for such a deny.
#[test]
fn the_slot_profile_has_none_of_the_denies_known_to_take_the_gpu() {
    for network in [Network::On, Network::Off] {
        let profile = generated(slot_profile(network));
        assert_eq!(
            breaking(&profile),
            Vec::<&str>::new(),
            "{network:?}:\n{profile}"
        );
        assert!(
            profile.lines().any(|l| l == "(allow default)"),
            "{network:?}:\n{profile}"
        );
    }
    for rule in NO_DEVICE_RULES.lines() {
        assert!(!breaks_the_gpu(rule), "{rule}");
    }
}

/// Catches the check above passing because it cannot fail: each of `ADDED`, appended to
/// the shipped profile text (`SLOT_PROFILE` and `SLOT_NO_NETWORK_PROFILE`), is the one
/// line the check then finds in what the profile builder emits, and so is each wider
/// form of the same denies; and the denies the profile does carry (`mach-register`,
/// `lsopen`, `file-mount`, `file-write*`, a `mach-lookup` of another service, an
/// `iokit-open` allow) are not found.
#[test]
fn a_deny_added_to_the_profile_is_found_in_what_the_builder_emits() {
    for base in [SLOT_PROFILE, SLOT_NO_NETWORK_PROFILE] {
        for deny in ADDED {
            let profile = generated(&WeakenedProfile::Add(deny).apply(base));
            assert_eq!(breaking(&profile), [deny], "{profile}");
        }
    }
    for deny in [
        "(deny default)",
        "(deny iokit*)",
        "(deny iokit-open (iokit-user-client-class \"AGXDeviceUserClient\"))",
        "(deny mach*)",
        "(deny file-write* iokit-open)",
        "(deny iokit-get-properties)",
        "(deny ipc-posix-shm*)",
        "(deny network* mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
        "(deny mach-lookup)",
        "(deny mach-lookup (global-name-prefix \"com.apple.\"))",
        "(deny mach-lookup (global-name-prefix \"com.apple.MTL\"))",
        "(deny mach-lookup (global-name-regex #\"[Mm][Tt][Ll]\"))",
        "(deny (with no-log) iokit-open)",
        "(deny\n    iokit-open)",
        "(deny (with no-log)\n    file-write* iokit-open\n    (iokit-user-client-class \"AGXDeviceUserClient\"))",
        "(deny mach-lookup (with no-report) (global-name \"com.apple.MTLCompilerService\"))",
        "(deny mach-lookup\n    (with no-report)\n    (global-name \"com.apple.MTLCompilerService\"))",
        "(deny mach-lookup (require-not (global-name \"com.apple.other\")))",
        "(deny mach-lookup (require-any (global-name \"com.apple.other\") (global-name \"com.apple.MTLCompilerService\")))",
        "(deny iokit-open ; ) (allow\n)",
    ] {
        assert!(breaks_the_gpu(deny), "{deny}");
        let profile = generated(&WeakenedProfile::Add(deny).apply(SLOT_PROFILE));
        assert_eq!(breaking(&profile), [deny], "{profile}");
    }
    for fine in [
        "(deny mach-register)",
        "(deny lsopen)",
        "(deny file-mount)",
        "(deny file-write*)",
        "(deny network*)",
        "(deny file-write* network*)",
        "(deny file-write* mach-lookup (global-name \"com.apple.other\"))",
        "(allow iokit-open)",
        "(allow mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
        "(deny mach-lookup (global-name \"com.apple.CoreDevice.CoreDeviceService\"))",
        "(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))",
        "(deny mach-lookup (global-name-prefix \"com.apple.MTLCompilerService.x\"))",
        "; (deny iokit-open)",
        "",
        "(deny (with no-log) file-write*)",
        "(deny mach-lookup (with no-report) (global-name \"com.apple.other\"))",
        "(deny file-write* (literal \"(deny iokit-open)\"))",
        "(deny file-write* ; iokit-open\n    (subpath \"/x\"))",
    ] {
        assert!(!breaks_the_gpu(fine), "{fine}");
    }
}
