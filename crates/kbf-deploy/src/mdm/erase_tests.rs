use std::path::Path;
use std::process::{Command, Stdio};

use super::*;
use kbf_host::exec::HostExec;
use kbf_host::path::Root;
use kbf_host::plan::DRY_RUN_NOTE;
use kbf_host::step::Step;
use kbf_host::testkit::{FakeExec, RUNNER, reply, scratch};
use kbf_mdm::signers::FLAG_USER_PRESENT;
use kbf_mdm::testkit::{SkKey, software_signature};

const KEY: &str = "/srv/op/id_erase_sk";
const OUT: &str = "/srv/op/erase.req";
const SIG: &str = "/srv/op/erase.req.sig";
const NOW: i64 = 1_800_000_000;
const NONCE: [u8; 16] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];
const MADE: Made = Made {
    now: NOW,
    nonce: NONCE,
};

/// The request `args()` asks for, written out by hand from the gate's format
/// (docs/design/mdm-backend.md, M4.2): NOW is 2027-01-15T08:00:00Z, plus 15 minutes.
const GOLDEN: &str = "kbf-erase-v1\n\
serial C02ABC123XYZ\n\
purpose erase-now\n\
reason leak after reboot\n\
nonce 0123456789abcdef0123456789abcdef\n\
not-after 2027-01-15T08:15:00Z\n";

/// OpenSSH's ECDSA security-key test key (openssh-portable regress/unittests/sshsig).
const ECDSA_SK_PUB: &str = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBKDVa5jRcT5V7E6ysmwVi7HJWh7p5D+hebLPakQcpnD2ajJZ6G/4WzuhlYWnclWY63JspDp299Rlhq5AT86/g8AAAAAEc3NoOg== ECDSA-SK test key";

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn args() -> EraseArgs {
    EraseArgs {
        serial: "C02ABC123XYZ".to_owned(),
        lease: None,
        reason: "leak after reboot".to_owned(),
        key: p(KEY),
        out: p(OUT),
        valid_for: 15,
        ssh_keygen: p(SSH_KEYGEN),
    }
}

fn file(bytes: &[u8]) -> Entry {
    Entry::File {
        perms: Perms::new(0o644),
        owner: RUNNER,
        bytes: bytes.to_vec(),
    }
}

/// An operator's host: the security key's public half at KEY.pub, and an `ssh-keygen`
/// played by `signer`, which signs standard input in software as the security key
/// would, with the flags byte `flags`.
fn host(signer: SkKey, flags: u8) -> FakeExec {
    let mut exec = FakeExec::new(501);
    let public = format!("{} operator@laptop\n", signer.openssh());
    exec.entries
        .insert(p(&format!("{KEY}.pub")), file(public.as_bytes()));
    exec.answer = Box::new(move |cmd| {
        let sig = signer.sign_with(NAMESPACE, &cmd.stdin, flags);
        Ok(reply(0, &sig, ""))
    });
    exec
}

fn run_on(exec: &mut FakeExec, a: &EraseArgs, mode: Mode) -> (Result<(), MdmError>, String) {
    let mut out = Vec::new();
    let result = run(exec, a, MADE, mode, &mut out);
    (result, String::from_utf8(out).expect("utf-8"))
}

fn bytes_at(exec: &FakeExec, path: &str) -> Vec<u8> {
    match exec.entries.get(&p(path)) {
        Some(Entry::File { bytes, .. }) => bytes.clone(),
        other => panic!("{path}: {other:?}"),
    }
}

/// Catches a request that is not the gate's `kbf-erase-v1` text line for line: two
/// lines swapped, a field renamed, a nonce not in lowercase hex, a `not-after` not
/// now plus --valid-for. The gate refuses such a text after the operator has touched
/// their key for it.
#[test]
fn the_signed_request_is_the_gates_text_line_for_line() {
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    let (result, _) = run_on(&mut exec, &args(), Mode::Apply);
    result.expect("signed");
    assert_eq!(String::from_utf8(bytes_at(&exec, OUT)).unwrap(), GOLDEN);

    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    let a = EraseArgs {
        lease: Some("lease-7".to_owned()),
        valid_for: 60,
        ..args()
    };
    run_on(&mut exec, &a, Mode::Apply).0.expect("signed");
    let want = GOLDEN
        .replace("purpose erase-now", "purpose privileged-lease lease-7")
        .replace("08:15:00Z", "09:00:00Z");
    assert_eq!(String::from_utf8(bytes_at(&exec, OUT)).unwrap(), want);
}

/// Catches a signing step that is not `ssh-keygen -Y sign -n kbf-mdm-erase -f KEY` on
/// the request's exact bytes with an empty environment (another namespace, the key
/// not passed, a variable leaked in, the bytes of another text signed), a request
/// written before its signature (a refused touch would leave an unsigned request),
/// and files another user may change.
#[test]
fn apply_signs_the_request_bytes_then_writes_the_request() {
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    let (result, out) = run_on(&mut exec, &args(), Mode::Apply);
    result.expect("signed");
    let [
        Step::Capture {
            cmd,
            path,
            perms,
            owner,
            ..
        },
        Step::WriteFile { path: req, .. },
    ] = exec.log.as_slice()
    else {
        panic!("{:?}", exec.log);
    };
    assert_eq!(cmd.program, p(SSH_KEYGEN));
    assert_eq!(cmd.args, ["-Y", "sign", "-n", "kbf-mdm-erase", "-f", KEY]);
    assert!(cmd.env.is_empty(), "{:?}", cmd.env);
    assert_eq!(cmd.stdin, GOLDEN.as_bytes());
    assert_eq!(cmd.user, None);
    assert_eq!((path, *perms, *owner), (&p(SIG), Perms::new(0o644), None));
    assert_eq!(req, &p(OUT));
    for line in [
        "touch the security key when it blinks",
        "checked as the gate checks it: sk-ssh-ed25519@openssh.com, touched, no PIN (a gate that requires one refuses it), counter 7",
        "send /srv/op/erase.req and /srv/op/erase.req.sig to the gate before 2027-01-15T08:15:00Z",
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
}

/// Catches a check after signing that reads the PIN flag from the wrong bit: the
/// operator is told whether a gate that requires the PIN will accept the signature.
#[test]
fn a_pin_verified_signature_says_so() {
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT | FLAG_USER_VERIFIED);
    let (result, out) = run_on(&mut exec, &args(), Mode::Apply);
    result.expect("signed");
    assert!(out.contains(", touched, PIN verified, counter 7"), "{out}");
}

/// Catches a pair the gate would refuse: its own verifier, from its allowed-signers
/// file, accepts what was written, with the user-verified requirement off and on.
#[test]
fn the_gate_verifies_and_parses_the_written_pair() {
    let key = SkKey::new(3);
    let line = key.allowed_line("ceo");
    let mut exec = host(key, FLAG_USER_PRESENT | FLAG_USER_VERIFIED);
    run_on(&mut exec, &args(), Mode::Apply).0.expect("signed");
    let (text, sig) = (bytes_at(&exec, OUT), bytes_at(&exec, SIG));
    let signers = AllowedSigners::parse(&line).expect("allowed signers");
    let sig = String::from_utf8(sig).unwrap();
    for require_pin in [false, true] {
        let signer = signers
            .verify(&text, &sig, NOW, require_pin)
            .expect("verifies");
        assert_eq!(signer.principals, "ceo");
    }
    let parsed = request::parse(std::str::from_utf8(&text).unwrap()).expect("parses");
    assert_eq!(parsed.purpose, Purpose::EraseNow);
    assert_eq!(parsed.not_after, NOW + 15 * 60);
}

/// `ssh-keygen -Y verify` (the real one, from OpenSSH) on `text` and `sig`, with
/// `allowed` as the allowed-signers file: whether it accepts, and what it said.
fn openssh_verifies(dir: &Path, allowed: &str, text: &[u8], sig: &[u8]) -> (bool, String) {
    let keygen = Path::new(SSH_KEYGEN);
    assert!(keygen.exists(), "the test needs OpenSSH's {SSH_KEYGEN}");
    std::fs::write(dir.join("allowed"), allowed).unwrap();
    std::fs::write(dir.join("req.sig"), sig).unwrap();
    let mut child = Command::new(keygen)
        .args(["-Y", "verify", "-I", "ceo", "-n", NAMESPACE, "-f"])
        .arg(dir.join("allowed"))
        .arg("-s")
        .arg(dir.join("req.sig"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ssh-keygen starts");
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), text).unwrap();
    let out = child.wait_with_output().unwrap();
    let said =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), said)
}

/// Catches a signed pair only this crate's checks accept: OpenSSH's own `ssh-keygen -Y
/// verify` must accept the security-key signature over the written request in the
/// gate's namespace (it cannot tell the software signer from a hardware key), and
/// refuse the same signature over a request with one byte changed.
#[test]
fn real_ssh_keygen_verify_accepts_the_signed_request() {
    let key = SkKey::new(3);
    let allowed = key.allowed_line("ceo");
    let mut exec = host(key, FLAG_USER_PRESENT);
    run_on(&mut exec, &args(), Mode::Apply).0.expect("signed");
    let (text, sig) = (bytes_at(&exec, OUT), bytes_at(&exec, SIG));
    let dir = scratch("kbf-deploy-erase-openssh");
    let (good, said) = openssh_verifies(&dir, &allowed, &text, &sig);
    assert!(good, "{said}");
    assert!(
        said.contains("Good \"kbf-mdm-erase\" signature for ceo"),
        "{said}"
    );
    let mut changed = text.clone();
    changed[text.len() - 2] = b'1';
    let (good, said) = openssh_verifies(&dir, &allowed, &changed, &sig);
    assert!(!good, "{said}");
}

/// Catches a signing argv the real `ssh-keygen` does not run as this verb needs: it
/// must sign the bytes on standard input and write the signature to standard output
/// (a file argument would make it write `<file>.sig` and print nothing; a missing
/// `-n` is refused). A software key stands in for the security key, which no CI host
/// has; the verb itself refuses software keys before signing.
#[test]
fn the_signing_argv_signs_standard_input_with_the_real_ssh_keygen() {
    let dir = scratch("kbf-deploy-erase-argv");
    let key = dir.join("k");
    let made = Command::new(SSH_KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .status()
        .expect("ssh-keygen starts");
    assert!(made.success());
    let key = p(key.to_str().unwrap());
    let cmd = sign_cmd(&p(SSH_KEYGEN), &key, GOLDEN);
    let out = HostExec::new(Root::host())
        .query(&cmd)
        .expect("ssh-keygen runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sig = (ARMORED.apply)(&out.stdout).expect("an armored signature on stdout");
    let public = std::fs::read_to_string(format!("{key}.pub")).unwrap();
    let allowed = format!("ceo namespaces=\"{NAMESPACE}\" {public}");
    let (good, said) = openssh_verifies(&dir, &allowed, GOLDEN.as_bytes(), &sig);
    assert!(good, "{said}");
}

/// Catches a dry run that signs or writes (the key would ask for a touch, and a
/// request would exist), or does not show the operator what would be signed.
#[test]
fn a_dry_run_prints_the_request_and_the_plan_and_runs_nothing() {
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    let (result, out) = run_on(&mut exec, &args(), Mode::DryRun);
    result.expect("planned");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
    assert!(exec.queries.borrow().is_empty());
    assert_eq!(exec.entries.len(), 1, "only the public key");
    let want = format!(
        "erase request for C02ABC123XYZ, acceptable until 2027-01-15T08:15:00Z:\n\
         {}\
         DRY-RUN: run /usr/bin/ssh-keygen -Y sign -n kbf-mdm-erase -f {KEY} (stdin {n} bytes) > {SIG} (0644, through an SSH signature)\n\
         DRY-RUN: write {OUT} (0644, {n} bytes)\n\
         {DRY_RUN_NOTE}\n",
        GOLDEN
            .lines()
            .map(|l| format!("    {l}\n"))
            .collect::<String>(),
        n = GOLDEN.len(),
    );
    assert_eq!(out, want);
}

/// Catches a verb that signs as root: a root-owned request on the operator's machine,
/// and a security key prompt under sudo.
#[test]
fn apply_as_root_is_refused_before_signing() {
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    exec.euid = 0;
    let (result, _) = run_on(&mut exec, &args(), Mode::Apply);
    let e = result.expect_err("refused");
    assert!(
        matches!(e, MdmError::Deploy(kbf_host::HostError::IsRoot)),
        "{e}"
    );
    assert_eq!(e.exit_code(), 2);
    assert!(exec.log.is_empty());
}

/// Each refusal, and that it comes before anything is signed or written. Catches a
/// dropped check: a request the gate refuses (bad serial, reason or lease) signed
/// anyway, a validity the gate refuses (over an hour) or none, a software key used
/// (the gate accepts only security keys), and a signed request overwritten.
#[test]
fn each_refusal_comes_before_anything_is_signed() {
    type Edit = fn(&mut EraseArgs, &mut FakeExec);
    let rows: [(&str, Edit, &str); 13] = [
        (
            "valid for 0",
            |a, _| a.valid_for = 0,
            "--valid-for 0: a request is valid for 1 to 60 minutes",
        ),
        ("valid for 61", |a, _| a.valid_for = 61, "--valid-for 61"),
        (
            "serial",
            |a, _| a.serial = "C02-ABC".to_owned(),
            "the gate would refuse this request: bad serial",
        ),
        (
            "empty reason",
            |a, _| a.reason = String::new(),
            "bad reason",
        ),
        (
            "reason with a tab",
            |a, _| a.reason = "a\tb".to_owned(),
            "bad reason",
        ),
        (
            "lease",
            |a, _| a.lease = Some("bad/lease".to_owned()),
            "bad purpose",
        ),
        (
            "no public key",
            |_, e| {
                e.entries.clear();
            },
            "/srv/op/id_erase_sk.pub does not exist",
        ),
        (
            "public key is a directory",
            |_, e| {
                e.entries.insert(
                    p("/srv/op/id_erase_sk.pub"),
                    Entry::Dir {
                        perms: Perms::new(0o755),
                        owner: RUNNER,
                    },
                );
            },
            "/srv/op/id_erase_sk.pub is not an OpenSSH public key: not a file",
        ),
        (
            "public key garbage",
            |_, e| {
                e.entries
                    .insert(p("/srv/op/id_erase_sk.pub"), file(b"not a key\n"));
            },
            "/srv/op/id_erase_sk.pub is not an OpenSSH public key: ",
        ),
        (
            "software key",
            |_, e| {
                let (line, _) = software_signature("ceo", b"x");
                let public = line.split_once("\" ").unwrap().1.to_owned();
                e.entries
                    .insert(p("/srv/op/id_erase_sk.pub"), file(public.as_bytes()));
            },
            "/srv/op/id_erase_sk.pub is a ssh-ed25519 key; the gate accepts only security keys",
        ),
        (
            "request exists",
            |_, e| {
                e.entries.insert(p(OUT), file(b"old"));
            },
            "/srv/op/erase.req already exists: a signed request is never overwritten",
        ),
        (
            "signature exists",
            |_, e| {
                e.entries.insert(p(SIG), file(b"old"));
            },
            "/srv/op/erase.req.sig already exists",
        ),
        (
            "unreadable",
            |a, _| a.out = p("/unreadable"),
            "/unreadable: ",
        ),
    ];
    for (name, edit, want) in rows {
        let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
        let mut a = args();
        edit(&mut a, &mut exec);
        let (result, out) = run_on(&mut exec, &a, Mode::Apply);
        let e = result.expect_err(name);
        assert!(e.to_string().contains(want), "{name}: {e}");
        let code = if name == "unreadable" { 1 } else { 2 };
        assert_eq!(e.exit_code(), code, "{name}");
        assert!(exec.log.is_empty(), "{name}: {:?}", exec.log);
        assert!(out.is_empty(), "{name}: {out}");
    }
}

/// Catches validity bounds off by one: 1 and 60 minutes are what the gate accepts,
/// and an ECDSA security key is a key the gate accepts too.
#[test]
fn the_bounds_and_both_security_key_types_are_accepted() {
    for valid_for in [1, MAX_VALID_FOR] {
        let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
        let a = EraseArgs {
            valid_for,
            ..args()
        };
        run_on(&mut exec, &a, Mode::DryRun).0.expect("planned");
    }
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    let public = p("/srv/op/id_erase_sk.pub");
    exec.entries.insert(public, file(ECDSA_SK_PUB.as_bytes()));
    run_on(&mut exec, &args(), Mode::DryRun)
        .0
        .expect("an ecdsa-sk key");
}

/// The fake `ssh-keygen` answers with `stdout` instead of a signature by the key.
fn answering(stdout: impl Fn(&[u8]) -> String + 'static) -> FakeExec {
    let mut exec = host(SkKey::new(3), FLAG_USER_PRESENT);
    exec.answer = Box::new(move |cmd| Ok(reply(0, &stdout(&cmd.stdin), "")));
    exec
}

/// Catches a signed pair accepted here that the gate refuses: no touch, another key
/// than KEY.pub, another namespace, a software key. Each fails the check after
/// signing, exit 1, naming the gate's reason.
#[test]
fn a_signature_the_gate_refuses_fails_the_check() {
    let rows: [(&str, FakeExec, VerifyError); 4] = [
        ("no touch", host(SkKey::new(3), 0), VerifyError::NoTouch),
        (
            "another key",
            answering(|m| SkKey::new(4).sign(m)),
            VerifyError::UnknownSigner,
        ),
        (
            "another namespace",
            answering(|m| SkKey::new(3).sign_with("file", m, FLAG_USER_PRESENT)),
            VerifyError::Namespace("file".to_owned()),
        ),
        (
            "software key",
            answering(|m| software_signature("ceo", m).1),
            VerifyError::SoftwareKey("ssh-ed25519".to_owned()),
        ),
    ];
    for (name, mut exec, want) in rows {
        let (result, out) = run_on(&mut exec, &args(), Mode::Apply);
        let e = result.expect_err(name);
        assert_eq!(e.exit_code(), 1, "{name}");
        match e {
            MdmError::Erase(EraseError::Check { why, .. }) => {
                assert_eq!(why, CheckError::Verify(want), "{name}");
            }
            other => panic!("{name}: {other}"),
        }
        assert!(!out.contains("send "), "{name}: {out}");
    }
}

/// Catches a signing step that leaves a request behind when the key was not touched
/// (ssh-keygen exits non-zero) or ssh-keygen printed no signature: nothing is written,
/// exit 1.
#[test]
fn a_failed_signing_writes_nothing() {
    let mut refused = host(SkKey::new(3), FLAG_USER_PRESENT);
    refused.answer = Box::new(|_| Ok(reply(255, "", "Signing (stdin) failed: device not found")));
    for (name, mut exec) in [
        ("exit 255", refused),
        ("no signature", answering(|_| "hello\n".to_owned())),
    ] {
        let (result, _) = run_on(&mut exec, &args(), Mode::Apply);
        let e = result.expect_err(name);
        assert_eq!(e.exit_code(), 1, "{name}: {e}");
        assert_eq!(exec.entries.len(), 1, "{name}: only the public key");
    }
}

/// Files on the fake host: path and bytes.
type Files<'a> = &'a [(&'a str, &'a [u8])];

/// Catches a check after signing that trusts what it did not read back: a file
/// missing, a request on disk other than the one asked for, bytes that are not text.
#[test]
fn the_pair_is_checked_as_read_back() {
    let key = SkKey::new(3);
    let public = PublicKey::from_openssh(&key.openssh()).unwrap();
    let request = request::parse(GOLDEN).unwrap();
    let sig = key.sign(GOLDEN.as_bytes());
    let check = |entries: Files| {
        let mut exec = FakeExec::new(501);
        for (path, bytes) in entries {
            exec.entries.insert(p(path), file(bytes));
        }
        check_pair(&exec, &request, &public, (&p(OUT), &p(SIG)), NOW)
    };
    let other = GOLDEN.replace("nonce 0123", "nonce 3210");
    let cases: [(Files, CheckError); 5] = [
        (&[(SIG, sig.as_bytes())], CheckError::Missing(p(OUT))),
        (&[(OUT, GOLDEN.as_bytes())], CheckError::Missing(p(SIG))),
        (
            &[(OUT, other.as_bytes()), (SIG, sig.as_bytes())],
            CheckError::Differs,
        ),
        (
            &[(OUT, b"\xff"), (SIG, sig.as_bytes())],
            CheckError::NotText("the request"),
        ),
        (
            &[(OUT, GOLDEN.as_bytes()), (SIG, b"\xff")],
            CheckError::NotText("the signature"),
        ),
    ];
    for (entries, want) in cases {
        assert_eq!(check(entries), Err(want));
    }
    let good = check(&[(OUT, GOLDEN.as_bytes()), (SIG, sig.as_bytes())]).expect("checks");
    assert_eq!(good.flags, FLAG_USER_PRESENT);
}
