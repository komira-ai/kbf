//! The allowlist of channels: what a channel may name, what is refused as invalid or unsafe,
//! and whose attestation a channel may rely on.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;

use crate::channel::{Allowlist, DEFAULT_SOURCE_REF, Source, is_branch_ref};
use crate::outcome::Refusal;
use crate::release::Origin;
use crate::request::Channel;
use crate::trust::Trust;

/// A scratch directory the trust of a test reaches, an allowlist path and a pinned-sums file in it.
struct Fx {
    dir: TestDir,
    trust: Trust,
    allowlist: PathBuf,
    sums: PathBuf,
}

impl Fx {
    fn new(name: &str) -> Fx {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid(), &dir);
        let sums = dir.join("stable.SHA256SUMS");
        write(&sums, 0o644, b"");
        Fx {
            trust,
            allowlist: dir.join("host-install.toml"),
            sums,
            dir,
        }
    }
}

/// A file with  and .
fn write(path: &std::path::Path, mode: u32, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn allowlist(fx: &Fx, text: &str) -> Result<Allowlist, Refusal> {
    write(&fx.allowlist, 0o644, text.as_bytes());
    Allowlist::load(&fx.trust, &fx.allowlist)
}

fn stable() -> Channel {
    Channel::new("stable").unwrap()
}

#[test]
fn a_channel_names_a_drop_directory_and_pinned_sums() {
    let fx = Fx::new("chan-channel");
    let list = allowlist(
        &fx,
        &format!(
            "[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\n",
            fx.sums.display()
        ),
    )
    .unwrap();
    let channel = list.channel(&fx.trust, &stable()).unwrap();
    assert_eq!(channel.source, Source::Dir("/d".into()));
    assert_eq!(channel.sums, Some(fx.sums.clone()));
    assert_eq!(channel.source_ref, DEFAULT_SOURCE_REF);
    let list = allowlist(
        &fx,
        &format!("[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\nsource_ref = \"refs/heads/release/1\"\n", fx.sums.display()),
    )
    .unwrap();
    assert_eq!(
        list.channel(&fx.trust, &stable()).unwrap().source_ref,
        "refs/heads/release/1"
    );
    assert_eq!(
        list.channel(&fx.trust, &Channel::new("beta").unwrap())
            .map_err(|e| e.refusal()),
        Err(Refusal::ChannelNotAllowed)
    );
}

/// A channel needs no sums: the attestation is the trust anchor. Sums that are named are
/// still checked to be root's, and a channel without them reads as having none.
#[test]
fn a_channel_may_have_no_sums() {
    let fx = Fx::new("chan-channel-nosums");
    let list = allowlist(&fx, "[channels.stable]\ndir = \"/d\"\n").unwrap();
    let channel = list.channel(&fx.trust, &stable()).unwrap();
    assert_eq!(
        (channel.source, channel.sums, channel.source_ref.as_str()),
        (Source::Dir("/d".into()), None, DEFAULT_SOURCE_REF)
    );
    // Named sums that others can write are unsafe; sums that are not there are invalid.
    let list = allowlist(
        &fx,
        &format!(
            "[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\n",
            fx.sums.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fx.sums, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        list.channel(&fx.trust, &stable()).map_err(|e| e.refusal()),
        Err(Refusal::ConfigUnsafe)
    );
}

#[test]
fn an_allowlist_that_is_not_plain_is_refused() {
    let fx = Fx::new("chan-channel-bad");
    let s = fx.sums.display();
    for (why, text) in [
        ("not toml", "channels = ".to_owned()),
        ("no channels", String::new()),
        (
            "an unknown key",
            format!("[channels.stable]\ndir = \"/d\"\nsums = \"{s}\"\nurl = \"x\"\n"),
        ),
        ("no dir", format!("[channels.stable]\nsums = \"{s}\"\n")),
        (
            "a relative dir",
            format!("[channels.stable]\ndir = \"d\"\nsums = \"{s}\"\n"),
        ),
        (
            "a relative sums",
            "[channels.stable]\ndir = \"/d\"\nsums = \"s\"\n".to_owned(),
        ),
        (
            "a control character",
            format!("[channels.stable]\ndir = \"/d\\u0007\"\nsums = \"{s}\"\n"),
        ),
        (
            "a bad channel name",
            format!("[channels.Stable]\ndir = \"/d\"\nsums = \"{s}\"\n"),
        ),
        (
            "a bad ref",
            format!("[channels.stable]\ndir = \"/d\"\nsums = \"{s}\"\nsource_ref = \"main\"\n"),
        ),
    ] {
        assert_eq!(
            allowlist(&fx, &text).err(),
            Some(Refusal::ConfigInvalid),
            "{why}"
        );
    }
}

#[test]
fn the_allowlist_and_the_pinned_sums_must_be_roots() {
    let fx = Fx::new("chan-channel-trust");
    let text = format!(
        "[channels.stable]\ndir = \"/d\"\nsums = \"{}\"\n",
        fx.sums.display()
    );
    write(&fx.allowlist, 0o666, text.as_bytes());
    assert_eq!(
        Allowlist::load(&fx.trust, &fx.allowlist).err(),
        Some(Refusal::ConfigUnsafe)
    );
    let list = allowlist(&fx, &text).unwrap();
    fs::set_permissions(
        &fx.sums,
        std::os::unix::fs::PermissionsExt::from_mode(0o664),
    )
    .unwrap();
    assert_eq!(
        list.channel(&fx.trust, &stable()).map_err(|e| e.refusal()),
        Err(Refusal::ConfigUnsafe)
    );
    fs::remove_file(&fx.sums).unwrap();
    assert_eq!(
        list.channel(&fx.trust, &stable()).map_err(|e| e.refusal()),
        Err(Refusal::ConfigInvalid)
    );
    fs::remove_file(&fx.allowlist).unwrap();
    assert_eq!(
        Allowlist::load(&fx.trust, &fx.allowlist).err(),
        Some(Refusal::ConfigInvalid)
    );
}

#[test]
fn a_source_ref_is_a_branch_that_is_safe_as_an_argument() {
    for good in [
        "refs/heads/main",
        "refs/heads/release/1.2",
        "refs/heads/a_b-c",
    ] {
        assert!(is_branch_ref(good), "{good}");
    }
    for bad in [
        "main",
        "refs/tags/v1",
        "refs/heads/",
        "refs/heads//a",
        "refs/heads/a//b",
        "refs/heads/a/",
        "refs/heads/a..b",
        "refs/heads/a b",
        "refs/heads//x",
        "refs/heads/a;b",
    ] {
        assert!(!is_branch_ref(bad), "{bad}");
    }
}

// ---- GitHub channels ----

fn stable_allowlist(fx: &Fx, body: &str) -> Result<Allowlist, Refusal> {
    write(
        &fx.allowlist,
        0o644,
        format!("[channels.stable]\n{body}").as_bytes(),
    );
    Allowlist::load(&fx.trust, &fx.allowlist)
}

/// A channel is a directory or GitHub, never both and never neither; a token file belongs to
/// GitHub; sums and the branch go with either.
#[test]
fn a_channel_is_a_directory_or_github_never_both() {
    let fx = Fx::new("chan-gh-config");
    let list = stable_allowlist(&fx, "source = \"github\"\n").unwrap();
    let channel = list.channel(&fx.trust, &stable()).unwrap();
    assert_eq!(channel.source, Source::Github { token_file: None });
    assert_eq!(channel.sums, None);

    let token = fx.dir.join("github.token");
    write(&token, 0o600, b"tok");
    let sums = fx.sums.display();
    let list = stable_allowlist(
        &fx,
        &format!(
            "source = \"github\"\ntoken_file = \"{}\"\nsums = \"{sums}\"\nsource_ref = \"refs/heads/release/1\"\n",
            token.display()
        ),
    )
    .unwrap();
    let channel = list.channel(&fx.trust, &stable()).unwrap();
    assert_eq!(
        channel.source,
        Source::Github {
            token_file: Some(token.clone())
        }
    );
    assert_eq!(channel.sums, Some(fx.sums.clone()));
    assert_eq!(channel.source_ref, "refs/heads/release/1");

    for (why, body) in [
        ("both", "dir = \"/d\"\nsource = \"github\"\n"),
        ("neither", "source_ref = \"refs/heads/main\"\n"),
        ("another source", "source = \"s3\"\n"),
        ("a source spelled otherwise", "source = \"GitHub\"\n"),
        (
            "a token file with a directory",
            "dir = \"/d\"\ntoken_file = \"/t\"\n",
        ),
        (
            "a relative token file",
            "source = \"github\"\ntoken_file = \"t\"\n",
        ),
        (
            "a token file that is empty text",
            "source = \"github\"\ntoken_file = \"\"\n",
        ),
    ] {
        assert_eq!(
            stable_allowlist(&fx, body).err(),
            Some(Refusal::ConfigInvalid),
            "{why}"
        );
    }
}

/// A token file that others can write is not trusted: the channel is not given out.
#[test]
fn a_token_file_others_can_write_is_unsafe() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new("chan-gh-token-unsafe");
    let token = fx.dir.join("github.token");
    write(&token, 0o600, b"tok");
    let list = stable_allowlist(
        &fx,
        &format!(
            "source = \"github\"\ntoken_file = \"{}\"\n",
            token.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        list.channel(&fx.trust, &stable()).map_err(|e| e.refusal()),
        Err(Refusal::ConfigUnsafe)
    );
    fs::remove_file(&token).unwrap();
    assert!(list.channel(&fx.trust, &stable()).is_err());
}

// ---- the channel's repository and signer workflow ----

const OTHER: &str = "owner/other";
const OTHER_WORKFLOW: &str = "owner/other/.github/workflows/release.yml";

/// The whole allowlist file, top-level keys included.
fn whole_allowlist(fx: &Fx, text: &str) -> Result<Allowlist, Refusal> {
    write(&fx.allowlist, 0o644, text.as_bytes());
    Allowlist::load(&fx.trust, &fx.allowlist)
}

fn other_origin() -> Origin {
    Origin::new(OTHER, OTHER_WORKFLOW, "kbf").unwrap()
}

/// A signer other than kbf's own is refused when the channel is used unless the allowlist's
/// `trusted_workflows` names that exact workflow; naming another repository's workflow
/// does not help.
#[test]
fn another_signer_is_refused_at_use_unless_the_allowlist_trusts_it() {
    let fx = Fx::new("chan-gh-trust");
    let channel_text = format!(
        "[channels.stable]\nsource = \"github\"\nrepository = \"{OTHER}\"\nsigner_workflow = \"{OTHER_WORKFLOW}\"\n"
    );
    let refused = |list: &Allowlist| list.channel(&fx.trust, &stable()).unwrap_err();

    let list = whole_allowlist(&fx, &channel_text).unwrap();
    let error = refused(&list);
    assert_eq!(error.refusal(), Refusal::ChannelNotAllowed);
    let words = error.to_string();
    assert!(
        words.contains("stable")
            && words.contains(OTHER_WORKFLOW)
            && words.contains("trusted_workflows"),
        "{words}"
    );

    let list = whole_allowlist(
        &fx,
        &format!(
            "trusted_workflows = [\"owner/unrelated/.github/workflows/release.yml\", \"{OTHER}/.github/workflows/other.yml\"]\n{channel_text}"
        ),
    )
    .unwrap();
    assert_eq!(refused(&list).refusal(), Refusal::ChannelNotAllowed);

    let list = whole_allowlist(
        &fx,
        &format!(
            "trusted_workflows = [\"owner/unrelated/.github/workflows/release.yml\", \"{OTHER_WORKFLOW}\"]\n{channel_text}"
        ),
    )
    .unwrap();
    let channel = list.channel(&fx.trust, &stable()).unwrap();
    assert_eq!(channel.origin, other_origin());

    // kbf's own signer is never refused, with or without a listing.
    let list = whole_allowlist(&fx, "[channels.stable]\nsource = \"github\"\n").unwrap();
    assert_eq!(
        list.channel(&fx.trust, &stable()).unwrap().origin,
        Origin::new(
            "komira-ai/kbf",
            "komira-ai/kbf/.github/workflows/artifacts.yml",
            "kbf",
        )
        .unwrap()
    );
}

/// A repository, workflow or prefix that is not safe to put in a URL or an argument of `gh`
/// is invalid when the allowlist is read; so is a repository without a workflow, a prefix
/// where the file names are fixed, and a trusted repository that is not one.
#[test]
fn a_bad_repository_workflow_or_prefix_makes_the_allowlist_invalid() {
    let fx = Fx::new("chan-gh-bad-origin");
    let ok_workflow = "owner/other/.github/workflows/release.yml";
    let with = |keys: &str| format!("[channels.stable]\nsource = \"github\"\n{keys}");
    let mut bad: Vec<(String, String)> = Vec::new();
    for repository in [
        "owner",
        "owner/",
        "/name",
        "a/b/c",
        "-owner/name",
        "owner/-name",
        "owner/.name",
        "owner/na..me",
        "owner/na me",
        "owner/na;me",
        "owner/na\"me",
        "own_er/name",
        "owner/name?x=1",
        "",
    ] {
        bad.push((
            format!("repository \"{repository}\""),
            with(&format!(
                "repository = \"{repository}\"\nsigner_workflow = \"{repository}/.github/workflows/release.yml\"\n"
            )),
        ));
    }
    for workflow in [
        "owner/other/.github/workflows/",
        "owner/other/.github/workflows/.yml",
        "owner/other/.github/workflows/release.sh",
        "owner/other/.github/workflows/sub/release.yml",
        "owner/other/.github/workflows/../release.yml",
        "owner/other/.github/workflows/-release.yml",
        "owner/other/.github/workflows/re lease.yml",
        "owner/other/.github/release.yml",
        "owner/another/.github/workflows/release.yml",
        "owner/other.github/workflows/release.yml",
        "owner/other/.github/workflows/release.yml@refs/heads/main",
        "komira-ai/kbf/.github/workflows/artifacts.yml",
    ] {
        bad.push((
            format!("workflow \"{workflow}\""),
            with(&format!(
                "repository = \"{OTHER}\"\nsigner_workflow = \"{workflow}\"\n"
            )),
        ));
    }
    bad.push((
        "repository alone".into(),
        with(&format!("repository = \"{OTHER}\"\n")),
    ));
    bad.push((
        "workflow alone".into(),
        with(&format!("signer_workflow = \"{ok_workflow}\"\n")),
    ));
    for prefix in ["", "-x", "Kbf", "a b", "a/b", "a_b", &"a".repeat(33)] {
        bad.push((
            format!("prefix \"{prefix}\""),
            with(&format!("artifact_prefix = \"{prefix}\"\n")),
        ));
    }
    bad.push((
        "prefix of a directory channel".into(),
        "[channels.stable]\ndir = \"/d\"\nartifact_prefix = \"other\"\n".into(),
    ));
    bad.push((
        "prefix of a channel with sums".into(),
        format!(
            "[channels.stable]\nsource = \"github\"\nsums = \"{}\"\nartifact_prefix = \"other\"\n",
            fx.sums.display()
        ),
    ));
    for trusted in [
        "\"owner/other\"",
        "\"owner/other/.github/workflows/\"",
        "\"owner/other/.github/workflows/a.sh\"",
        "\"owner/other/.github/workflows/sub/a.yml\"",
        "\"-o/n/.github/workflows/a.yml\"",
        "\"x/.github/workflows/a.yml\"",
        "1",
        "\"\"",
    ] {
        bad.push((
            format!("trusted {trusted}"),
            format!("trusted_workflows = [{trusted}]\n{}", with("")),
        ));
    }
    for (why, text) in &bad {
        assert_eq!(
            whole_allowlist(&fx, text).err(),
            Some(Refusal::ConfigInvalid),
            "{why}"
        );
    }

    // Each of the good forms, for contrast.
    for text in [
        with(&format!(
            "repository = \"{OTHER}\"\nsigner_workflow = \"{ok_workflow}\"\nartifact_prefix = \"other-2\"\n"
        )),
        with(
            "repository = \"Owner-1/na.me_2\"\nsigner_workflow = \"Owner-1/na.me_2/.github/workflows/a-b_c.v2.yaml\"\n",
        ),
        format!("trusted_workflows = []\n{}", with("")),
    ] {
        assert!(whole_allowlist(&fx, &text).is_ok(), "{text}");
    }
}

/// A file without the new keys means what it always meant: kbf's repository, workflow and
/// file names, and kbf's former name still accepted.
#[test]
fn a_channel_without_the_new_keys_is_kbfs() {
    let fx = Fx::new("chan-gh-default-origin");
    let list = whole_allowlist(&fx, "[channels.stable]\nsource = \"github\"\n").unwrap();
    let origin = list.channel(&fx.trust, &stable()).unwrap().origin;
    assert_eq!(origin.repository(), "komira-ai/kbf");
    assert_eq!(
        origin.workflow(),
        "komira-ai/kbf/.github/workflows/artifacts.yml"
    );
    assert_eq!(origin.prefix(), "kbf");
    assert_eq!(
        origin.signers(),
        [
            (
                "komira-ai/kbf",
                "komira-ai/kbf/.github/workflows/artifacts.yml"
            ),
            (
                "komira-ai/komira-build-farm",
                "komira-ai/komira-build-farm/.github/workflows/artifacts.yml"
            ),
        ]
    );
}

/// kbf's own repository is trusted only with its own signer workflow: a channel that names
/// another workflow of `komira-ai/kbf` is refused when it is used, with no allow-list entry.
#[test]
fn another_workflow_of_kbfs_repository_is_refused_at_use() {
    let fx = Fx::new("chan-gh-kbf-ci");
    let list = whole_allowlist(
        &fx,
        "[channels.stable]\nsource = \"github\"\nrepository = \"komira-ai/kbf\"\nsigner_workflow = \"komira-ai/kbf/.github/workflows/ci.yml\"\n",
    )
    .unwrap();
    let error = list.channel(&fx.trust, &stable()).unwrap_err();
    assert_eq!(error.refusal(), Refusal::ChannelNotAllowed);
    assert!(error.to_string().contains("ci.yml"), "{error}");

    // Listing that exact workflow allows it; listing the repository's default does not.
    let channel_text = "[channels.stable]\nsource = \"github\"\nrepository = \"komira-ai/kbf\"\nsigner_workflow = \"komira-ai/kbf/.github/workflows/ci.yml\"\n";
    let other = "trusted_workflows = [\"komira-ai/kbf/.github/workflows/artifacts.yml\"]\n";
    let list = whole_allowlist(&fx, &format!("{other}{channel_text}")).unwrap();
    assert!(list.channel(&fx.trust, &stable()).is_err());
    let own = "trusted_workflows = [\"komira-ai/kbf/.github/workflows/ci.yml\"]\n";
    let list = whole_allowlist(&fx, &format!("{own}{channel_text}")).unwrap();
    assert!(list.channel(&fx.trust, &stable()).is_ok());
}
