//! A channel whose source is GitHub: the release is picked from the listing of the
//! repository's releases, fetched as `nobody`, and then checked as the files of a directory
//! are (`stage_tests`).

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::rc::Rc;

use super::fx::{
    Fx, INSTALL, NEW, NEW_VERSION, OLD, OLD_VERSION, Sim, binary, mkdir, mode, tarball, write,
};
use crate::mac::channel::{MacAllowlist, MacChannel, Source};
use crate::mac::fetch::{FetchError, MAX_PAGE_BYTES, MAX_PAGES, PER_PAGE, page_body};
use crate::mac::release::{Origin, bundle_name, tarball_name};
use crate::mac::stage::{Staged, stage};
use crate::outcome::{FailureCode, Outcome, Refusal};
use crate::request::{Channel, Version};
use crate::runner::{Cmd, Identity};

/// Another commit that starts with the same 12 digits as `NEW`.
const NEW_TWIN: &str = "bbbbbbbbbbbbcccccccccccccccccccccccccccc";

fn asset(name: &str, size: usize) -> String {
    format!(
        r#"{{"name":"{name}","size":{size},"state":"uploaded","url":"https://evil.example.invalid/api/{name}","browser_download_url":"https://evil.example.invalid/download/{name}"}}"#
    )
}

/// A release of the listing, as GitHub prints one, with the files a release holds for each
/// commit.
fn release(tag: &str, commits: &[&str]) -> String {
    let assets: Vec<String> = commits
        .iter()
        .flat_map(|c| {
            [
                asset(&tarball_name(c), 1000),
                asset(&bundle_name(c), 100),
                asset(&format!("kbf-{c}-darwin-arm64.version.json"), 100),
                asset(&format!("kbf-{c}-linux-x86_64.tar.gz"), 1000),
                asset(&format!("kbf-installer-{c}-darwin-arm64.tar.gz"), 1000),
            ]
        })
        .collect();
    format!(
        r#"{{"tag_name":"{tag}","draft":false,"prerelease":false,"url":"https://evil.example.invalid/r","assets":[{}]}}"#,
        assets.join(",")
    )
}

/// A release no test asks for.
fn filler(i: usize) -> String {
    let commit = format!("1{i:039x}");
    release(&format!("v0.0.{i}"), &[&commit])
}

fn page(releases: &[String]) -> String {
    format!("[{}]", releases.join(","))
}

/// A page that is full of releases no test asks for, then `extra` ones.
fn full_page(n: usize, extra: &[String]) -> String {
    let mut releases: Vec<String> = (0..PER_PAGE - extra.len())
        .map(|i| filler(n * 1000 + i))
        .collect();
    releases.extend(extra.iter().cloned());
    page(&releases)
}

fn publish(sim: &Sim, tag: &str, commit: &str, binary: &[u8]) {
    let tar = tarball(&[("kbf", tar::EntryType::Regular, binary)]);
    let mut assets = sim.assets.borrow_mut();
    let files = assets.entry(tag.to_owned()).or_default();
    files.insert(tarball_name(commit), tar);
    files.insert(bundle_name(commit), b"{\"bundle\":1}\n".to_vec());
}

/// GitHub holds release `NEW` as `v1.0` on the one page of the listing.
fn github(fx: &Fx) -> Sim {
    let sim = fx.sim();
    sim.pages
        .borrow_mut()
        .push(page(&[release("v1.0", &[NEW])]));
    publish(&sim, "v1.0", NEW, &fx.new_binary);
    sim
}

fn channel(fx: &Fx, pinned: bool) -> MacChannel {
    MacChannel {
        source: Source::Github { token_file: None },
        sums: pinned.then(|| fx.sums.clone()),
        source_ref: "refs/heads/main".into(),
        origin: Origin::kbf(),
    }
}

fn run(fx: &Fx, sim: &Sim, channel: &MacChannel, version: &str) -> Result<Staged, Outcome> {
    mkdir(&fx.host.work, 0o711);
    stage(&fx.host, sim, channel, &Version::new(version).unwrap())
}

fn refusal(result: Result<Staged, Outcome>) -> Refusal {
    match result {
        Err(Outcome::Refused(r) | Outcome::RefusedBecause(r, _)) => r,
        other => panic!("not a refusal: {other:?}"),
    }
}

/// The calls of `curl`, which lists the releases.
fn curl_calls(fx: &Fx, sim: &Sim) -> Vec<Cmd> {
    sim.calls
        .borrow()
        .iter()
        .filter(|c| c.program == fx.host.curl.to_str().unwrap())
        .cloned()
        .collect()
}

/// The URL a call of `curl` asked for.
fn url_of(call: &Cmd) -> String {
    call.args.last().unwrap().clone()
}

/// The calls of `gh` that ran `verb` (`release`, `attestation`).
fn gh_calls(fx: &Fx, sim: &Sim, verb: &str) -> Vec<Cmd> {
    sim.calls
        .borrow()
        .iter()
        .filter(|c| c.program == fx.host.gh.to_str().unwrap() && c.args[0] == verb)
        .cloned()
        .collect()
}

fn me(fx: &Fx) -> Identity {
    let meta = fs::metadata(&*fx.dir).unwrap();
    Identity {
        uid: meta.uid(),
        gid: meta.gid(),
    }
}

/// Nothing was left behind and nothing outside the stage changed.
fn assert_untouched(fx: &Fx) {
    assert!(!fx.host.stage().exists(), "the stage is removed");
    assert_eq!(fx.status(), "");
    assert!(!fx.host.apply_plist().exists());
    assert_eq!(
        fx.link(&fx.host.current()).as_deref(),
        Some(format!("releases/{OLD}").as_str())
    );
    assert_eq!(fx.link(&fx.host.previous()), None);
}

// ---- the choice of the release ----

/// The version's digits pick the one release across the pages, an old release included: the
/// first two pages are full and hold nothing of it, the third holds it, and the listing is
/// read until an empty page and no further.
#[test]
fn the_listing_picks_the_unique_asset_across_pages() {
    let fx = Fx::new("mac-gh-pages");
    let sim = fx.sim();
    {
        let mut pages = sim.pages.borrow_mut();
        pages.push(full_page(1, &[release("v9.0", &[NEW])]));
        pages.push(full_page(2, &[]));
        pages.push(page(&[filler(7), release("v0.1", &[OLD])]));
    }
    publish(&sim, "v9.0", NEW, &fx.new_binary);
    publish(&sim, "v0.1", OLD, &binary("old"));
    let staged = run(&fx, &sim, &channel(&fx, false), OLD_VERSION).unwrap();
    assert_eq!(staged.commit, OLD);
    let listed: Vec<String> = curl_calls(&fx, &sim).iter().map(url_of).collect();
    assert_eq!(
        listed,
        [1, 2, 3, 4].map(|n| format!(
            "https://api.github.com/repos/komira-ai/kbf/releases?per_page=100&page={n}"
        ))
    );
    let download = &gh_calls(&fx, &sim, "release")[0];
    assert_eq!(download.args[2], "v0.1");

    // The release on the first page is picked the same way.
    let fx = Fx::new("mac-gh-pages-new");
    let sim = fx.sim();
    sim.pages
        .borrow_mut()
        .push(full_page(1, &[release("v9.0", &[NEW])]));
    sim.pages.borrow_mut().push(page(&[filler(7)]));
    publish(&sim, "v9.0", NEW, &fx.new_binary);
    assert_eq!(
        run(&fx, &sim, &channel(&fx, false), NEW_VERSION)
            .unwrap()
            .commit,
        NEW
    );
    assert_eq!(curl_calls(&fx, &sim).len(), 3);
}

/// A page with fewer than 100 releases is not the end of the listing: a second commit that
/// starts with the same digits on the page after it still makes the release ambiguous.
#[test]
fn a_short_page_does_not_end_the_listing() {
    let fx = Fx::new("mac-gh-short-page");
    let sim = github(&fx);
    sim.pages.borrow_mut().push(page(&[release(
        "v2.0",
        &[&format!("bbbbbbbbbbbb{}", "c".repeat(28))],
    )]));
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::AmbiguousRelease);
    assert!(gh_calls(&fx, &sim, "release").is_empty());
    assert_untouched(&fx);
}

/// No asset of a commit that starts with the digits is an unknown release, however many
/// pages say so; nothing is downloaded and nothing is left.
#[test]
fn a_version_no_asset_starts_with_is_an_unknown_release() {
    let fx = Fx::new("mac-gh-unknown");
    for pages in [
        Vec::new(),
        vec![page(&[filler(1)])],
        vec![full_page(1, &[]), page(&[filler(2)])],
    ] {
        let sim = fx.sim();
        *sim.pages.borrow_mut() = pages;
        let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
        assert_eq!(refusal(result), Refusal::UnknownRelease);
        assert!(gh_calls(&fx, &sim, "release").is_empty());
        assert_untouched(&fx);
    }
    // Assets that only look like the tarball are not it.
    let sim = fx.sim();
    let upper = NEW.to_uppercase();
    let names = [
        format!("{}.sig", tarball_name(NEW)),
        format!("kbf-{}-darwin-arm64.tar.gz", &NEW[..39]),
        format!("kbf-{NEW}-linux-x86_64.tar.gz"),
        format!("kbf-installer-{NEW}-darwin-arm64.tar.gz"),
        format!("Kbf-{NEW}-darwin-arm64.tar.gz"),
        format!("kbf-{upper}-darwin-arm64.tar.gz"),
        format!("kbf-{NEW}-darwin-arm64.tar"),
    ];
    let assets: Vec<String> = names.iter().map(|n| asset(n, 10)).collect();
    let near = format!(
        r#"{{"tag_name":"v2.0","draft":false,"assets":[{}]}}"#,
        assets.join(",")
    );
    sim.pages.borrow_mut().push(page(&[near]));
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::UnknownRelease);
}

/// Two different commits that start with the digits are an ambiguous release, also when
/// they are on different pages; the same commit in two releases is one release, the newest.
#[test]
fn two_commits_with_the_digits_are_an_ambiguous_release() {
    let fx = Fx::new("mac-gh-ambiguous");
    let sim = fx.sim();
    sim.pages
        .borrow_mut()
        .push(full_page(1, &[release("v3.0", &[NEW])]));
    sim.pages
        .borrow_mut()
        .push(page(&[release("v2.0", &[NEW_TWIN])]));
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::AmbiguousRelease);
    assert!(gh_calls(&fx, &sim, "release").is_empty());
    assert_untouched(&fx);

    let sim = fx.sim();
    sim.pages
        .borrow_mut()
        .push(page(&[release("v3.0", &[NEW]), release("v2.0", &[NEW])]));
    publish(&sim, "v3.0", NEW, &fx.new_binary);
    publish(
        &sim,
        "v2.0",
        NEW,
        &binary("another build of the same commit"),
    );
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
    assert_eq!(gh_calls(&fx, &sim, "release")[0].args[2], "v3.0");
}

/// A listing that is never exhausted is read up to the page limit and no further; with no
/// match in it that is a failed fetch (not "unknown": not all of it was seen), and a match
/// in it is taken.
#[test]
fn the_listing_is_read_up_to_the_page_limit() {
    let fx = Fx::new("mac-gh-limit");
    let sim = fx.sim();
    for n in 1..=MAX_PAGES as usize + 3 {
        sim.pages.borrow_mut().push(full_page(n, &[]));
    }
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_eq!(curl_calls(&fx, &sim).len(), MAX_PAGES as usize);
    assert_untouched(&fx);

    let sim = fx.sim();
    for n in 1..=MAX_PAGES as usize + 3 {
        let extra = if n == 4 {
            vec![release("v5.0", &[NEW])]
        } else {
            vec![]
        };
        sim.pages.borrow_mut().push(full_page(n, &extra));
    }
    publish(&sim, "v5.0", NEW, &fx.new_binary);
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
    assert_eq!(curl_calls(&fx, &sim).len(), MAX_PAGES as usize);
}

/// The download is rebuilt from the tag and the asset names, whatever URL the listing
/// gives; an entry whose tag could be taken for an option is not used.
#[test]
fn a_url_in_the_listing_is_ignored_and_the_download_is_rebuilt() {
    let fx = Fx::new("mac-gh-url");
    let sim = github(&fx);
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
    let download = &gh_calls(&fx, &sim, "release")[0];
    let scratch = fx.host.stage().join("fetch").display().to_string();
    assert_eq!(
        download.args,
        [
            "release",
            "download",
            "v1.0",
            "--repo",
            "komira-ai/kbf",
            "--pattern",
            &tarball_name(NEW),
            "--pattern",
            &bundle_name(NEW),
            "--dir",
            &scratch,
        ]
    );
    for call in sim.calls.borrow().iter() {
        let text = format!("{} {}", call.program, call.args.join(" "));
        assert!(!text.contains("evil.example.invalid"), "{text}");
        if call.program == fx.host.curl.to_str().unwrap() {
            assert!(url_of(call).starts_with("https://api.github.com/repos/"));
        } else if call.args[0] != "attestation" {
            assert!(!text.contains("http"), "{text}");
        }
    }

    for tag in ["--repo", "-x", ".hidden", "v1 0", "v1.0\\nx"] {
        let fx = Fx::new("mac-gh-tag");
        let sim = fx.sim();
        sim.pages.borrow_mut().push(page(&[release(tag, &[NEW])]));
        publish(&sim, tag, NEW, &fx.new_binary);
        let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
        assert_eq!(refusal(result), Refusal::UnknownRelease, "{tag}");
        assert!(gh_calls(&fx, &sim, "release").is_empty(), "{tag}");
    }
}

// ---- the fetch ----

/// The listing and the download run as `nobody`, in the home `nobody` owns, with the
/// stage open to traverse while they run; the download has a time limit and a limit on
/// what it writes, both from the host's tuning. The proxy variables are not set, and the
/// copy in the stage is made after, by the root process.
#[test]
fn the_fetch_runs_as_nobody_with_a_size_cap_and_a_time_limit() {
    let fx = Fx::new("mac-gh-nobody");
    let sim = github(&fx);
    let seen = Rc::new(RefCell::new(Vec::new()));
    {
        let (seen, stage_dir) = (seen.clone(), fx.host.stage());
        *sim.before.borrow_mut() = Some(Box::new(move |cmd| {
            let scratch =
                fs::metadata(stage_dir.join("fetch")).map(|m| (m.mode() & 0o777, m.uid()));
            let stage_mode = fs::metadata(&stage_dir).map_or(0, |m| m.mode() & 0o777);
            let verb = if cmd.program.ends_with("/curl") {
                "list".to_owned()
            } else {
                cmd.args[0].clone()
            };
            seen.borrow_mut().push((verb, stage_mode, scratch.ok()));
        }));
    }
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
    let uid = me(&fx).uid;
    let seen = seen.borrow();
    let at = |verb: &str| seen.iter().find(|s| s.0 == verb).unwrap().clone();
    assert_eq!(at("list").1, 0o711);
    assert_eq!(at("list").2, Some((0o700, uid)));
    assert_eq!(at("release").1, 0o711);
    assert_eq!(at("release").2, Some((0o700, uid)));

    let tuning = fx.host.tuning;
    let home = fx.host.stage().join("gh-home").display().to_string();
    let list = &curl_calls(&fx, &sim)[0];
    let download = &gh_calls(&fx, &sim, "release")[0];
    assert_eq!(list.program, fx.host.curl.display().to_string());
    assert_eq!(download.program, fx.host.gh.display().to_string());
    for call in [list, download] {
        assert_eq!(call.run_as, Some(me(&fx)));
        assert!(
            call.env
                .iter()
                .all(|(k, _)| !k.to_lowercase().contains("proxy") && k != "GH_TOKEN")
        );
    }
    let env: Vec<(&str, &str)> = download
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert!(env.contains(&("HOME", home.as_str())), "{env:?}");
    assert!(env.contains(&("GH_CONFIG_DIR", home.as_str())), "{env:?}");
    assert_eq!(list.timeout, Some(tuning.tool));
    assert_eq!(download.timeout, Some(tuning.fetch));
    let cap = download.write_cap.as_ref().unwrap();
    assert_eq!(cap.dir, fx.host.stage().join("fetch"));
    assert_eq!(cap.max, tuning.tarball_max + tuning.bundle_max);

    // The copy is root's, in the stage, and the scratch is gone from the sealed stage.
    let stage_dir = fx.host.stage();
    assert_eq!(mode(&stage_dir), 0o700);
    assert!(!stage_dir.join("fetch").exists());
    assert_eq!(
        fs::read(stage_dir.join(tarball_name(NEW))).unwrap(),
        tarball(&[("kbf", tar::EntryType::Regular, &fx.new_binary)])
    );
}

/// A file over the limit is refused before anything else: when the listing says so, no
/// download is run; when the download writes more than the listing said, the copy refuses it.
/// Nothing is left staged and no root state has changed.
#[test]
fn an_oversize_release_file_is_refused_and_nothing_is_staged() {
    let fx = Fx::new("mac-gh-oversize");
    let sim = fx.sim();
    let big = release("v1.0", &[NEW]).replace(
        &format!(r#""name":"{}","size":1000"#, tarball_name(NEW)),
        &format!(
            r#""name":"{}","size":{}"#,
            tarball_name(NEW),
            fx.host.tuning.tarball_max + 1
        ),
    );
    sim.pages.borrow_mut().push(page(&[big]));
    publish(&sim, "v1.0", NEW, &fx.new_binary);
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert!(gh_calls(&fx, &sim, "release").is_empty());
    assert_untouched(&fx);

    let sim = github(&fx);
    sim.break_("download-big");
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_eq!(gh_calls(&fx, &sim, "release").len(), 1);
    assert_untouched(&fx);

    // A bundle that is larger than the listing said is refused by the copy.
    let sim = github(&fx);
    sim.break_("download-big-bundle");
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_untouched(&fx);

    // The bundle has its own, smaller limit.
    let fx = Fx::new("mac-gh-oversize-bundle");
    let sim = fx.sim();
    let big = release("v1.0", &[NEW]).replace(
        &format!(r#""name":"{}","size":100"#, bundle_name(NEW)),
        &format!(
            r#""name":"{}","size":{}"#,
            bundle_name(NEW),
            fx.host.tuning.bundle_max + 1
        ),
    );
    sim.pages.borrow_mut().push(page(&[big]));
    publish(&sim, "v1.0", NEW, &fx.new_binary);
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_untouched(&fx);
}

/// A release that lists no bundle for the tarball, and a listing page far larger than any
/// page of releases, are refused as a failed fetch.
#[test]
fn a_release_without_its_bundle_and_an_absurd_page_are_refused() {
    let fx = Fx::new("mac-gh-nobundle");
    let sim = fx.sim();
    let without = release("v1.0", &[NEW]).replace(&bundle_name(NEW), "other.sigstore.json");
    sim.pages.borrow_mut().push(page(&[without]));
    publish(&sim, "v1.0", NEW, &fx.new_binary);
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert!(gh_calls(&fx, &sim, "release").is_empty());
    assert_untouched(&fx);

    let sim = fx.sim();
    sim.pages
        .borrow_mut()
        .push(format!("[{}]", " ".repeat(MAX_PAGE_BYTES)));
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_untouched(&fx);
}

/// A download that outlives its limit (the runner kills it, leaving what it wrote) is a
/// refusal, and so is a listing page that does; the partial file is not left staged.
#[test]
fn a_fetch_that_times_out_is_refused_and_nothing_is_staged() {
    let fx = Fx::new("mac-gh-timeout");
    let sim = github(&fx);
    sim.break_("download-timeout");
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_untouched(&fx);

    let sim = github(&fx);
    sim.break_("list-timeout");
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert!(gh_calls(&fx, &sim, "release").is_empty());
    assert_untouched(&fx);
}

/// A network failure (of the listing, of the download), an answer that is not a listing, a
/// release that lacks the file, all refuse with `fetch_failed` through the whole verb, with
/// exit status 2 and no change to the kit, the status or the launchd jobs.
#[test]
fn a_network_failure_refuses_before_any_root_state_changes() {
    for broken in ["list-net", "list-junk", "download-net"] {
        let fx = Fx::new("mac-gh-net");
        write_channel(&fx, "source = \"github\"\n");
        let sim = github(&fx);
        sim.break_(broken);
        let answer = fx.handle(&sim, INSTALL);
        assert_eq!(answer.code, 2, "{broken}: {}", answer.err);
        assert_eq!(
            answer.json(),
            serde_json::json!({"result":"refused","reason":"fetch_failed"}),
            "{broken}"
        );
        assert_untouched(&fx);
        assert_eq!(sim.count("/bin/launchctl bootstrap"), 0);
        assert_eq!(sim.count("/bin/launchctl bootout"), 0);
        let audit = fx.audit_lines();
        assert_eq!(audit[0]["outcome"], "refused");
        assert_eq!(audit[0]["reason"], "fetch_failed");
    }
    // The release is listed and has no such file.
    let fx = Fx::new("mac-gh-missing-file");
    let sim = github(&fx);
    sim.assets.borrow_mut().clear();
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::FetchFailed);
    assert_untouched(&fx);
}

/// Catches a listing that a signal ended being reported as "exit status None": the refusal
/// is `fetch_failed`, and its reason names the program and the signal, so the operator can tell
/// a kill (SIGKILL), a crash (SIGSEGV, SIGABRT) and a broken pipe (SIGPIPE) apart.
#[test]
fn a_listing_ended_by_a_signal_names_the_signal() {
    let fx = Fx::new("mac-gh-signal");
    let sim = github(&fx);
    sim.break_("list-signal");
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    match result {
        Err(Outcome::RefusedBecause(Refusal::FetchFailed, why)) => {
            assert!(why.starts_with("curl: signal 13 (SIGPIPE): "), "{why}");
            assert!(!why.contains("None"), "{why}");
        }
        other => panic!("not a fetch failure with a reason: {other:?}"),
    }
    assert_untouched(&fx);
}

fn write_channel(fx: &Fx, body: &str) {
    write(
        &fx.host.allowlist,
        0o644,
        format!("[channels.stable]\n{body}").as_bytes(),
    );
}

/// The verb end to end: a channel with GitHub as its source fetches, checks and launches the
/// apply job, and answers as for a directory.
#[test]
fn an_install_from_github_launches_the_apply_job() {
    let fx = Fx::new("mac-gh-install");
    write_channel(&fx, "source = \"github\"\n");
    let sim = github(&fx);
    let answer = fx.handle(&sim, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(
        answer.json(),
        serde_json::json!({"result":"apply_started","version":NEW_VERSION,"commit":NEW,"apply":"running"})
    );
    assert_eq!(fx.status(), "running\n");
    assert_eq!(mode(&fx.host.stage()), 0o700);
    assert_eq!(
        fs::read(fx.host.stage().join("kbf")).unwrap(),
        fx.new_binary
    );
}

// ---- what is checked after the fetch ----

/// The attestation is checked offline on the copy in the stage, bound to the full commit,
/// after the fetch and never in its place; one that fails refuses the release.
#[test]
fn the_attestation_is_still_checked_offline_after_a_fetch() {
    let fx = Fx::new("mac-gh-attest");
    let sim = github(&fx);
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
    let verbs: Vec<String> = sim
        .calls
        .borrow()
        .iter()
        .filter(|c| c.program == fx.host.gh.to_str().unwrap())
        .map(|c| c.args[0].clone())
        .collect();
    assert_eq!(verbs, ["release", "attestation"]);
    assert_eq!(curl_calls(&fx, &sim).len(), 2);
    let attest = &gh_calls(&fx, &sim, "attestation")[0];
    let args = attest.args.join(" ");
    let stage_dir = fx.host.stage();
    assert!(args.contains(&stage_dir.join(tarball_name(NEW)).display().to_string()));
    assert!(args.contains(&format!(
        "--bundle {}",
        stage_dir.join(bundle_name(NEW)).display()
    )));
    assert!(args.contains(&format!("--source-digest {NEW}")));
    assert!(args.contains("--deny-self-hosted-runners"));
    assert!(
        attest
            .env
            .iter()
            .any(|(k, v)| k == "HTTPS_PROXY" && v.contains("127.0.0.1"))
    );
    assert_eq!(attest.run_as, Some(me(&fx)));

    let fx = Fx::new("mac-gh-attest-refused");
    let sim = github(&fx);
    sim.break_("gh-current");
    sim.break_("gh");
    let result = run(&fx, &sim, &channel(&fx, false), NEW_VERSION);
    match result {
        Err(Outcome::Failed(f)) => {
            assert_eq!(f.code, FailureCode::VerifyFailed);
            assert!(f.detail.contains("not attested"), "{}", f.detail);
        }
        other => panic!("{other:?}"),
    }
    assert!(!fx.host.stage().exists());
}

/// Pinned sums still apply to a fetched release: the commit is the one they list, and the
/// tarball's sha256 is the listed one.
#[test]
fn pinned_sums_still_apply_to_a_fetched_release() {
    let fx = Fx::new("mac-gh-sums");
    let sim = github(&fx);
    let staged = run(&fx, &sim, &channel(&fx, true), NEW_VERSION).unwrap();
    assert_eq!(staged.commit, NEW);

    // A tarball that is not the listed one.
    let fx = Fx::new("mac-gh-sums-wrong");
    let sim = github(&fx);
    publish(&sim, "v1.0", NEW, &binary("a binary the sums do not list"));
    match run(&fx, &sim, &channel(&fx, true), NEW_VERSION) {
        Err(Outcome::Failed(f)) => {
            assert_eq!(f.code, FailureCode::VerifyFailed);
            assert!(f.detail.contains("not the pinned one"), "{}", f.detail);
        }
        other => panic!("{other:?}"),
    }
    assert!(!fx.host.stage().exists());

    // Sums that list no such release: nothing is fetched at all.
    let fx = Fx::new("mac-gh-sums-unlisted");
    let sim = github(&fx);
    write(
        &fx.sums,
        0o644,
        format!("{}  {}\n", "0".repeat(64), tarball_name(OLD)).as_bytes(),
    );
    let result = run(&fx, &sim, &channel(&fx, true), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::UnknownRelease);
    assert!(curl_calls(&fx, &sim).is_empty());

    // Sums that list two commits with the digits are ambiguous, as in a directory's.
    let fx = Fx::new("mac-gh-sums-ambiguous");
    let sim = github(&fx);
    write(
        &fx.sums,
        0o644,
        format!(
            "{}  {}\n{}  {}\n",
            "1".repeat(64),
            tarball_name(NEW),
            "2".repeat(64),
            tarball_name(NEW_TWIN)
        )
        .as_bytes(),
    );
    let result = run(&fx, &sim, &channel(&fx, true), NEW_VERSION);
    assert_eq!(refusal(result), Refusal::AmbiguousRelease);
    assert!(curl_calls(&fx, &sim).is_empty());

    // Of two commits with the digits in the listing, the sums name the one.
    let fx = Fx::new("mac-gh-sums-twin");
    let sim = fx.sim();
    sim.pages.borrow_mut().push(page(&[
        release("v2.0", &[NEW_TWIN]),
        release("v1.0", &[NEW]),
    ]));
    publish(&sim, "v1.0", NEW, &fx.new_binary);
    publish(&sim, "v2.0", NEW_TWIN, &binary("twin"));
    let staged = run(&fx, &sim, &channel(&fx, true), NEW_VERSION).unwrap();
    assert_eq!(staged.commit, NEW);
}

/// A channel with a directory is as before: no call to GitHub, the files come from the
/// directory.
#[test]
fn a_directory_channel_never_talks_to_github() {
    let fx = Fx::new("mac-gh-dir");
    let sim = fx.sim();
    mkdir(&fx.host.work, 0o711);
    let dir = MacChannel {
        source: Source::Dir(fx.channel_dir.clone()),
        sums: Some(fx.sums.clone()),
        source_ref: "refs/heads/main".into(),
        origin: Origin::kbf(),
    };
    stage(&fx.host, &sim, &dir, &Version::new(NEW_VERSION).unwrap()).unwrap();
    assert!(curl_calls(&fx, &sim).is_empty());
    assert!(gh_calls(&fx, &sim, "release").is_empty());
    assert!(!fx.host.stage().join("fetch").exists());
}

/// The system `curl` that lists the releases is held to the same standard as the pinned `gh`:
/// a writable one is refused for a GitHub channel and nothing is run. A directory channel
/// never uses `curl`, so it is not held to it.
#[test]
fn a_writable_curl_is_refused_only_for_a_github_channel() {
    let fx = Fx::new("mac-gh-curl-trust");
    let sim = github(&fx);
    fs::set_permissions(&fx.host.curl, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        refusal(run(&fx, &sim, &channel(&fx, false), NEW_VERSION)),
        Refusal::KeyUntrusted
    );
    assert!(sim.calls.borrow().is_empty());
    let sim = fx.sim();
    mkdir(&fx.host.work, 0o711);
    let dir = MacChannel {
        source: Source::Dir(fx.channel_dir.clone()),
        sums: Some(fx.sums.clone()),
        source_ref: "refs/heads/main".into(),
        origin: Origin::kbf(),
    };
    stage(&fx.host, &sim, &dir, &Version::new(NEW_VERSION).unwrap()).unwrap();
}

// ---- the listing with curl ----

/// With no token the listing is the system `curl`, as `nobody`: the arguments are exactly
/// these (`-q` first so no configuration file is read, HTTPS only, no redirect, the status
/// and headers printed with the body, a time and a size limit, the fixed URL last), the command carries no environment of
/// its own (the runner adds only its fixed `PATH`, `LANG` and `DEBIAN_FRONTEND`), nothing is
/// on its standard input, and nothing like a token is anywhere in the call.
#[test]
fn the_listing_runs_curl_as_nobody_with_no_token_and_fixed_flags() {
    let fx = Fx::new("mac-gh-curl");
    let sim = github(&fx);
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
    let calls = curl_calls(&fx, &sim);
    assert_eq!(calls.len(), 2);
    let tuning = fx.host.tuning;
    for (i, call) in calls.iter().enumerate() {
        assert_eq!(call.program, fx.host.curl.display().to_string());
        assert_eq!(
            call.args,
            [
                "-q",
                "--silent",
                "--show-error",
                "--include",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--max-redirs",
                "0",
                "--header",
                "Accept: application/vnd.github+json",
                "--header",
                "X-GitHub-Api-Version: 2022-11-28",
                "--user-agent",
                "kbf-host-install",
                "--max-time",
                &tuning.tool.as_secs().to_string(),
                "--max-filesize",
                &MAX_PAGE_BYTES.to_string(),
                "--url",
                &format!(
                    "https://api.github.com/repos/komira-ai/kbf/releases?per_page=100&page={}",
                    i + 1
                ),
            ]
        );
        assert!(call.env.is_empty(), "{:?}", call.env);
        assert!(call.stdin.is_empty());
        assert_eq!(call.run_as, Some(me(&fx)));
        assert_eq!(call.timeout, Some(tuning.tool));
        assert_eq!(call.stdout_max, Some(MAX_PAGE_BYTES));
        assert!(call.write_cap.is_none());
        let text = format!("{call:?}").to_lowercase();
        assert!(
            !text.contains("token") && !text.contains("bearer"),
            "{text}"
        );
    }
    // The download has no token either.
    assert!(
        gh_calls(&fx, &sim, "release")[0]
            .env
            .iter()
            .all(|(k, _)| k != "GH_TOKEN")
    );
}

/// Every answer of GitHub that is not a listing is `fetch_failed` through the whole verb:
/// exit status 2, nothing downloaded, nothing staged, no change to the kit, the status or
/// launchd; the audit line has the closed reason, and stderr says why. An exhausted rate
/// limit says when to retry and that a `token_file` raises the limit, whether the API shows it
/// by `x-ratelimit-remaining: 0`, by `retry-after` or by its message alone; a 403 that is
/// not a limit is an HTTP error like a 500.
#[test]
fn an_http_error_or_a_rate_limit_is_a_failed_fetch_and_stages_nothing() {
    let cases = [
        ("list-http", "GitHub answered HTTP 500: Server Error"),
        (
            "list-rate",
            "rate limited by GitHub, retry after 2026-09-21T14:13:20Z or set token_file",
        ),
        (
            "list-rate-wait",
            "rate limited by GitHub, retry after 60 seconds or set token_file",
        ),
        (
            "list-rate-message",
            "rate limited by GitHub, retry after a while or set token_file",
        ),
        ("list-forbidden", "GitHub answered HTTP 403: Forbidden"),
        ("list-nohttp", "the answer is not an HTTP response"),
    ];
    for (broken, why) in cases {
        let fx = Fx::new("mac-gh-http");
        write_channel(&fx, "source = \"github\"\n");
        let sim = github(&fx);
        sim.break_(broken);
        let answer = fx.handle(&sim, INSTALL);
        assert_eq!(answer.code, 2, "{broken}: {}", answer.err);
        assert_eq!(
            answer.json(),
            serde_json::json!({"result":"refused","reason":"fetch_failed"}),
            "{broken}"
        );
        assert_eq!(answer.err.trim(), why, "{broken}");
        assert_eq!(fx.audit_lines()[0]["reason"], "fetch_failed", "{broken}");
        assert_eq!(curl_calls(&fx, &sim).len(), 1, "{broken}");
        assert!(gh_calls(&fx, &sim, "release").is_empty(), "{broken}");
        assert_untouched(&fx);
        assert_eq!(sim.count("/bin/launchctl bootstrap"), 0);
    }
}

/// The pure reading of what `curl --include` printed: the body of a 200 (after interim
/// answers, with header names in any case), and the reason for everything else.
#[test]
fn a_listing_response_is_split_into_status_headers_and_body() {
    let ok = |raw: &str| page_body(raw.as_bytes()).map(<[u8]>::to_vec);
    assert_eq!(ok("HTTP/2 200 \r\nA: b\r\n\r\n[]"), Ok(b"[]".to_vec()));
    assert_eq!(ok("HTTP/1.1 200 OK\r\n\r\n[1]"), Ok(b"[1]".to_vec()));
    assert_eq!(
        ok("HTTP/1.1 100 Continue\r\n\r\nHTTP/2 200\r\nno colon here\r\n\r\n[]"),
        Ok(b"[]".to_vec())
    );
    for bad in [
        "",
        "[]",
        "HTTP/2 200\r\nno end of the headers",
        "HTTP/2\r\n\r\n[]",
        "HTTP/2 two\r\n\r\n[]",
        "FTP/2 200\r\n\r\n[]",
        "HTTP/1.1 100 Continue\r\n\r\nnothing after",
    ] {
        assert_eq!(
            ok(bad),
            Err(FetchError::Failed(
                "the answer is not an HTTP response".into()
            )),
            "{bad:?}"
        );
    }
    // Headers are read in any case, with and without a space after the colon.
    assert_eq!(
        ok("HTTP/2 403\r\nX-RateLimit-Remaining:0\r\nX-RateLimit-Reset: 0\r\n\r\n{}"),
        Err(FetchError::RateLimited(
            "rate limited by GitHub, retry after 1970-01-01T00:00:00Z or set token_file".into()
        ))
    );
    // A reset that is no time is not used; a 403 with quota left is no limit.
    assert_eq!(
        ok("HTTP/2 429\r\nx-ratelimit-reset: soon\r\nretry-after: soon\r\n\r\n{}"),
        Err(FetchError::RateLimited(
            "rate limited by GitHub, retry after a while or set token_file".into()
        ))
    );
    assert_eq!(
        ok("HTTP/2 403\r\nx-ratelimit-remaining: 5\r\n\r\nnot json"),
        Err(FetchError::Failed("GitHub answered HTTP 403".into()))
    );
    // Only 403 and 429 are limits; a message of any other status does not make one.
    assert_eq!(
        ok("HTTP/2 500\r\nx-ratelimit-remaining: 0\r\n\r\n{\"message\":\"API rate limit\"}"),
        Err(FetchError::Failed(
            "GitHub answered HTTP 500: API rate limit".into()
        ))
    );
    // The message is cut.
    let long = format!("HTTP/2 502\r\n\r\n{{\"message\":\"{}\"}}", "m".repeat(500));
    assert_eq!(
        ok(&long),
        Err(FetchError::Failed(format!(
            "GitHub answered HTTP 502: {}",
            "m".repeat(200)
        )))
    );
    // Output past the page limit is refused before it is read.
    let big = vec![b'x'; MAX_PAGE_BYTES + 1];
    assert_eq!(
        page_body(&big),
        Err(FetchError::Failed(
            "a page of the listing is too large".into()
        ))
    );
}

/// An interim `100 Continue` before the page does not hide it.
#[test]
fn an_interim_response_before_the_listing_is_skipped() {
    let fx = Fx::new("mac-gh-interim");
    let sim = github(&fx);
    sim.break_("list-interim");
    run(&fx, &sim, &channel(&fx, false), NEW_VERSION).unwrap();
}

// ---- the token ----

fn token_channel(fx: &Fx, token: &str) -> (MacChannel, std::path::PathBuf) {
    let file = fx.dir.join("github.token");
    write(&file, 0o600, token.as_bytes());
    let channel = MacChannel {
        source: Source::Github {
            token_file: Some(file.clone()),
        },
        ..channel(fx, false)
    };
    (channel, file)
}

/// The token of a channel is read by the root process from its file. The listing gets it
/// through curl's standard input only (never an argument or the environment), the download
/// as `GH_TOKEN`, and the attestation check not at all.
#[test]
fn the_token_goes_to_the_listing_by_stdin_and_the_download_only() {
    let fx = Fx::new("mac-gh-token");
    let sim = github(&fx);
    let (channel, _) = token_channel(&fx, "ghp_exampleToken-0\n");
    run(&fx, &sim, &channel, NEW_VERSION).unwrap();
    let has = |call: &Cmd| {
        call.env
            .iter()
            .any(|(k, v)| k == "GH_TOKEN" && v == "ghp_exampleToken-0")
    };
    for list in curl_calls(&fx, &sim) {
        assert_eq!(
            String::from_utf8(list.stdin.clone()).unwrap(),
            "header = \"Authorization: Bearer ghp_exampleToken-0\"\n"
        );
        assert_eq!(&list.args[list.args.len() - 4..][..2], ["--config", "-"]);
        assert!(list.env.is_empty(), "{:?}", list.env);
    }
    assert!(has(&gh_calls(&fx, &sim, "release")[0]));
    assert!(!has(&gh_calls(&fx, &sim, "attestation")[0]));
    for call in sim.calls.borrow().iter() {
        assert!(!call.args.iter().any(|a| a.contains("ghp_")), "{call:?}");
    }

    // A token that is not plain text is a bad allowlist, and nothing runs.
    for bad in ["", "two words", "tok\u{7}en"] {
        let fx = Fx::new("mac-gh-token-bad");
        let sim = github(&fx);
        let (channel, _) = token_channel(&fx, bad);
        assert_eq!(
            refusal(run(&fx, &sim, &channel, NEW_VERSION)),
            Refusal::ConfigInvalid,
            "{bad:?}"
        );
        assert!(sim.calls.borrow().is_empty());
    }
}

/// GitHub's workflow tokens are a prefix, an app id and a signed token of about 520 bytes in
/// base64url with dots. A channel's token file takes one, so a hosted runner's rehearsal can
/// list releases under the run's own quota; a token that is not one line of those bytes, or
/// is past 1024 bytes, is still refused.
#[test]
fn a_workflow_token_of_the_new_long_format_is_taken() {
    let long = format!(
        "ghs_123456_{}.{}.{}",
        "aB3-_".repeat(30),
        "xY9".repeat(60),
        "z_".repeat(60)
    );
    assert!(long.len() > 255 && long.len() <= 1024, "{}", long.len());
    let fx = Fx::new("mac-gh-token-long");
    let sim = github(&fx);
    let (channel, _) = token_channel(&fx, &format!("{long}\n"));
    run(&fx, &sim, &channel, NEW_VERSION).unwrap();
    for list in curl_calls(&fx, &sim) {
        assert_eq!(
            String::from_utf8(list.stdin.clone()).unwrap(),
            format!("header = \"Authorization: Bearer {long}\"\n")
        );
    }

    for bad in ["a".repeat(1025), "ghs_a/b".to_owned(), "ghs_a=b".to_owned()] {
        let fx = Fx::new("mac-gh-token-long-bad");
        let sim = github(&fx);
        let (channel, _) = token_channel(&fx, &bad);
        assert_eq!(
            refusal(run(&fx, &sim, &channel, NEW_VERSION)),
            Refusal::ConfigInvalid,
            "{} bytes",
            bad.len()
        );
        assert!(sim.calls.borrow().is_empty());
    }
}

// ---- the allowlist ----

fn allowlist(fx: &Fx, body: &str) -> Result<MacAllowlist, Refusal> {
    write_channel(fx, body);
    MacAllowlist::load(&fx.host.trust, &fx.host.allowlist)
}

fn stable() -> Channel {
    Channel::new("stable").unwrap()
}

/// A channel is a directory or GitHub, never both and never neither; a token file belongs to
/// GitHub; sums and the branch go with either.
#[test]
fn a_channel_is_a_directory_or_github_never_both() {
    let fx = Fx::new("mac-gh-config");
    let list = allowlist(&fx, "source = \"github\"\n").unwrap();
    let channel = list.channel(&fx.host.trust, &stable()).unwrap();
    assert_eq!(channel.source, Source::Github { token_file: None });
    assert_eq!(channel.sums, None);

    let token = fx.dir.join("github.token");
    write(&token, 0o600, b"tok");
    let sums = fx.sums.display();
    let list = allowlist(
        &fx,
        &format!(
            "source = \"github\"\ntoken_file = \"{}\"\nsums = \"{sums}\"\nsource_ref = \"refs/heads/release/1\"\n",
            token.display()
        ),
    )
    .unwrap();
    let channel = list.channel(&fx.host.trust, &stable()).unwrap();
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
            allowlist(&fx, body).err(),
            Some(Refusal::ConfigInvalid),
            "{why}"
        );
    }
}

/// A token file that others can write is not trusted: the channel is not given out.
#[test]
fn a_token_file_others_can_write_is_unsafe() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new("mac-gh-token-unsafe");
    let token = fx.dir.join("github.token");
    write(&token, 0o600, b"tok");
    let list = allowlist(
        &fx,
        &format!(
            "source = \"github\"\ntoken_file = \"{}\"\n",
            token.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        list.channel(&fx.host.trust, &stable())
            .map_err(|e| e.refusal()),
        Err(Refusal::ConfigUnsafe)
    );
    fs::remove_file(&token).unwrap();
    assert!(list.channel(&fx.host.trust, &stable()).is_err());
}

// ---- the channel's repository and signer workflow ----

const OTHER: &str = "owner/other";
const OTHER_WORKFLOW: &str = "owner/other/.github/workflows/release.yml";

/// The whole allowlist file, top-level keys included.
fn whole_allowlist(fx: &Fx, text: &str) -> Result<MacAllowlist, Refusal> {
    write(&fx.host.allowlist, 0o644, text.as_bytes());
    MacAllowlist::load(&fx.host.trust, &fx.host.allowlist)
}

fn other_origin() -> Origin {
    Origin::new(OTHER, OTHER_WORKFLOW, "kbf").unwrap()
}

fn other_channel(fx: &Fx) -> MacChannel {
    MacChannel {
        origin: other_origin(),
        ..channel(fx, false)
    }
}

/// A channel that names a repository is listed from that repository, downloaded from it and
/// attested for its workflow alone: `gh attestation verify` runs once, for that repository
/// and workflow, and kbf's former name is not tried as well.
#[test]
fn a_channel_of_another_repository_lists_downloads_and_attests_under_its_own_names() {
    let fx = Fx::new("mac-gh-other-repo");
    let sim = github(&fx);
    let staged = run(&fx, &sim, &other_channel(&fx), NEW_VERSION).unwrap();
    assert_eq!(staged.commit, NEW);

    let listed: Vec<String> = curl_calls(&fx, &sim).iter().map(url_of).collect();
    assert_eq!(
        listed[0],
        "https://api.github.com/repos/owner/other/releases?per_page=100&page=1"
    );
    let download = &gh_calls(&fx, &sim, "release")[0];
    let at = download.args.iter().position(|a| a == "--repo").unwrap();
    assert_eq!(download.args[at + 1], OTHER);

    let verifies = gh_calls(&fx, &sim, "attestation");
    assert_eq!(verifies.len(), 1);
    let args = &verifies[0].args;
    let value_of = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(value_of("--repo"), OTHER);
    assert_eq!(value_of("--signer-workflow"), OTHER_WORKFLOW);
    assert!(args.iter().any(|a| a == "--deny-self-hosted-runners"));
    assert_eq!(value_of("--source-ref"), "refs/heads/main");
    assert_eq!(value_of("--source-digest"), NEW);
}

/// A release attested by kbf's workflow is not accepted for a channel of another repository,
/// and the failing check is the one pair that channel names.
#[test]
fn a_channel_of_another_repository_does_not_accept_kbfs_former_name() {
    let fx = Fx::new("mac-gh-other-repo-fails");
    let sim = github(&fx);
    sim.break_("gh");
    let outcome = run(&fx, &sim, &other_channel(&fx), NEW_VERSION).unwrap_err();
    assert!(
        matches!(&outcome, Outcome::Failed(f) if f.code == FailureCode::VerifyFailed),
        "{outcome:?}"
    );
    assert_eq!(gh_calls(&fx, &sim, "attestation").len(), 1);
    assert_untouched(&fx);
}

/// The allowlist parses a channel of another repository and refuses it when it is used,
/// unless the same file lists its exact workflow in `trusted_workflows`; kbf's own signer
/// needs no listing, and a listing of another workflow (of the same repository or another)
/// does not help.
#[test]
fn another_signer_is_refused_at_use_unless_the_allowlist_trusts_it() {
    let fx = Fx::new("mac-gh-trust");
    let channel_text = format!(
        "[channels.stable]\nsource = \"github\"\nrepository = \"{OTHER}\"\nsigner_workflow = \"{OTHER_WORKFLOW}\"\n"
    );
    let refused = |list: &MacAllowlist| list.channel(&fx.host.trust, &stable()).unwrap_err();

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
    let channel = list.channel(&fx.host.trust, &stable()).unwrap();
    assert_eq!(channel.origin, other_origin());

    // kbf's own signer is never refused, with or without a listing.
    let list = whole_allowlist(&fx, "[channels.stable]\nsource = \"github\"\n").unwrap();
    assert_eq!(
        list.channel(&fx.host.trust, &stable()).unwrap().origin,
        Origin::kbf()
    );
}

/// A repository, workflow or prefix that is not safe to put in a URL or an argument of `gh`
/// is invalid when the allowlist is read; so is a repository without a workflow, a prefix
/// where the file names are fixed, and a trusted repository that is not one.
#[test]
fn a_bad_repository_workflow_or_prefix_makes_the_allowlist_invalid() {
    let fx = Fx::new("mac-gh-bad-origin");
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
    let fx = Fx::new("mac-gh-default-origin");
    let list = whole_allowlist(&fx, "[channels.stable]\nsource = \"github\"\n").unwrap();
    let origin = list.channel(&fx.host.trust, &stable()).unwrap().origin;
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
    let fx = Fx::new("mac-gh-kbf-ci");
    let list = whole_allowlist(
        &fx,
        "[channels.stable]\nsource = \"github\"\nrepository = \"komira-ai/kbf\"\nsigner_workflow = \"komira-ai/kbf/.github/workflows/ci.yml\"\n",
    )
    .unwrap();
    let error = list.channel(&fx.host.trust, &stable()).unwrap_err();
    assert_eq!(error.refusal(), Refusal::ChannelNotAllowed);
    assert!(error.to_string().contains("ci.yml"), "{error}");

    // Listing that exact workflow allows it; listing the repository's default does not.
    let channel_text = "[channels.stable]\nsource = \"github\"\nrepository = \"komira-ai/kbf\"\nsigner_workflow = \"komira-ai/kbf/.github/workflows/ci.yml\"\n";
    let other = "trusted_workflows = [\"komira-ai/kbf/.github/workflows/artifacts.yml\"]\n";
    let list = whole_allowlist(&fx, &format!("{other}{channel_text}")).unwrap();
    assert!(list.channel(&fx.host.trust, &stable()).is_err());
    let own = "trusted_workflows = [\"komira-ai/kbf/.github/workflows/ci.yml\"]\n";
    let list = whole_allowlist(&fx, &format!("{own}{channel_text}")).unwrap();
    assert!(list.channel(&fx.host.trust, &stable()).is_ok());
}
