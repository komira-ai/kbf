//! The fetch of a release from GitHub, against a fake `curl` and a fake `gh`: the release is
//! picked from the listing of the repository's releases and its two files are downloaded as the
//! unprivileged user; the commands are the fixed ones, and every failure is told apart.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::fetch::{
    AssetKind, FetchError, Fetcher, Limits, MAX_PAGE_BYTES, MAX_PAGES, PER_PAGE, page_body,
};
use crate::release::Origin;
use crate::runner::{Cmd, Exec, Identity, Runner};

const COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OLD_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// Another commit that starts with the same 12 digits as [`COMMIT`].
const TWIN: &str = "bbbbbbbbbbbbcccccccccccccccccccccccccccc";
const PLATFORM: &str = "linux-x86_64";
const WHO: Identity = Identity {
    uid: 4242,
    gid: 4343,
};
const LIMITS: Limits = Limits {
    tool: Duration::from_secs(7),
    download: Duration::from_secs(31),
    tarball_max: 5000,
    bundle_max: 500,
};
const CURL: &str = "/usr/bin/curl";
const GH: &str = "/usr/bin/gh";

fn tarball(commit: &str) -> String {
    format!("kbf-{commit}-{PLATFORM}.tar.gz")
}

fn bundle(commit: &str) -> String {
    format!("{}.sigstore.json", tarball(commit))
}

fn asset(name: &str, size: u64) -> String {
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
                asset(&tarball(c), 1000),
                asset(&bundle(c), 100),
                asset(&format!("kbf-{c}-{PLATFORM}.version.json"), 100),
                asset(&format!("kbf-{c}-darwin-arm64.tar.gz"), 1000),
                asset(&format!("kbf-installer-{c}-{PLATFORM}.tar.gz"), 1000),
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

fn exec(code: i32, stdout: &str, stderr: &str) -> io::Result<Exec> {
    Ok(Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

/// GitHub as `curl` and `gh` see it: the pages of the listing, the files of each release by tag,
/// the calls made, and the ways the answer can be made to fail.
#[derive(Default)]
struct Github {
    pages: RefCell<Vec<String>>,
    assets: RefCell<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    broken: RefCell<BTreeSet<&'static str>>,
    calls: RefCell<Vec<Cmd>>,
}

impl Github {
    /// GitHub holds [`COMMIT`] as `v1.0` on the one page of the listing.
    fn with_release() -> Github {
        let github = Github::default();
        github
            .pages
            .borrow_mut()
            .push(page(&[release("v1.0", &[COMMIT])]));
        github.publish("v1.0", COMMIT);
        github
    }

    fn publish(&self, tag: &str, commit: &str) {
        let mut assets = self.assets.borrow_mut();
        let files = assets.entry(tag.to_owned()).or_default();
        files.insert(tarball(commit), b"the tarball".to_vec());
        files.insert(bundle(commit), b"{\"bundle\":1}\n".to_vec());
    }

    fn break_(&self, what: &'static str) {
        self.broken.borrow_mut().insert(what);
    }

    fn is_broken(&self, what: &str) -> bool {
        self.broken.borrow().contains(what)
    }

    fn listing_calls(&self) -> Vec<Cmd> {
        let calls = self.calls.borrow();
        calls
            .iter()
            .filter(|c| c.program == CURL)
            .cloned()
            .collect()
    }

    fn download_calls(&self) -> Vec<Cmd> {
        let calls = self.calls.borrow();
        calls.iter().filter(|c| c.program == GH).cloned().collect()
    }

    fn curl_list(&self, cmd: &Cmd) -> io::Result<Exec> {
        let response = |status: &str, headers: &str, body: &str| {
            exec(0, &format!("HTTP/2 {status}\r\n{headers}\r\n{body}"), "")
        };
        if self.is_broken("list-http") {
            return response("500 ", "", "{\"message\":\"Server Error\"}");
        }
        if self.is_broken("list-rate") {
            return response(
                "403 ",
                "x-ratelimit-limit: 60\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 1790000000\r\n",
                "{\"message\":\"API rate limit exceeded for your address.\"}",
            );
        }
        if self.is_broken("list-rate-wait") {
            return response("429 ", "Retry-After: 60\r\n", "{}");
        }
        if self.is_broken("list-rate-message") {
            return response(
                "403 ",
                "x-ratelimit-remaining: 12\r\n",
                "{\"message\":\"You have exceeded a secondary rate limit.\"}",
            );
        }
        if self.is_broken("list-forbidden") {
            return response(
                "403 ",
                "x-ratelimit-remaining: 12\r\n",
                "{\"message\":\"Forbidden\"}",
            );
        }
        if self.is_broken("list-nohttp") {
            return exec(0, "<html>not a response</html>", "");
        }
        if self.is_broken("list-timeout") {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ran past its limit",
            ));
        }
        if self.is_broken("list-signal") {
            return Ok(Exec {
                code: None,
                signal: Some(13),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        if self.is_broken("list-net") {
            return exec(1, "", "error connecting to api.github.com");
        }
        if self.is_broken("list-junk") {
            return response("200 ", "", "<html>not json</html>");
        }
        let number: usize = cmd
            .args
            .last()
            .expect("the url")
            .rsplit("page=")
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a page number");
        let pages = self.pages.borrow();
        let interim = if self.is_broken("list-interim") {
            "HTTP/1.1 100 Continue\r\n\r\n"
        } else {
            ""
        };
        exec(
            0,
            &format!(
                "{interim}HTTP/2 200 \r\ncontent-type: application/json\r\n\r\n{}",
                pages.get(number - 1).map_or("[]", String::as_str)
            ),
            "",
        )
    }

    /// `gh release download <tag> --repo R --pattern A --pattern B --dir D`: writes the
    /// release's files that match into D.
    fn gh_download(&self, cmd: &Cmd) -> io::Result<Exec> {
        let args = &cmd.args;
        let tag = &args[2];
        let value_of = |flag: &str| {
            args.windows(2)
                .filter(|w| w[0] == flag)
                .map(|w| w[1].clone())
                .collect::<Vec<_>>()
        };
        let dir = PathBuf::from(&value_of("--dir")[0]);
        if self.is_broken("download-net") {
            return exec(1, "", "error connecting to api.github.com");
        }
        let assets = self.assets.borrow();
        let Some(files) = assets.get(tag) else {
            return exec(1, "", "release not found");
        };
        for name in value_of("--pattern") {
            let Some(bytes) = files.get(&name) else {
                return exec(1, "", &format!("no assets match {name}"));
            };
            std::fs::write(dir.join(&name), bytes).unwrap();
        }
        if self.is_broken("download-timeout") {
            // A real runner kills the command at its limit; what it wrote stays.
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ran past its limit",
            ));
        }
        exec(0, "", "")
    }
}

impl Runner for Github {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        self.calls.borrow_mut().push(cmd.clone());
        match cmd.program.as_str() {
            CURL => self.curl_list(cmd),
            GH => self.gh_download(cmd),
            other => Err(io::Error::other(format!("unscripted: {other}"))),
        }
    }
}

/// A scratch directory for the download.
struct Rig {
    dir: kbf_testkit::dir::TestDir,
    origin: Origin,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name);
        std::fs::create_dir(dir.join("scratch")).unwrap();
        Rig {
            dir,
            origin: Origin::new(
                "komira-ai/kbf",
                "komira-ai/kbf/.github/workflows/artifacts.yml",
                "kbf",
            )
            .unwrap(),
        }
    }

    fn scratch(&self) -> PathBuf {
        self.dir.join("scratch")
    }

    fn fetcher<'a>(&'a self, github: &'a Github, token: Option<&'a str>) -> Fetcher<'a> {
        Fetcher {
            platform: PLATFORM,
            asset: AssetKind::Tarball,
            origin: &self.origin,
            runner: github,
            curl: Path::new(CURL),
            gh: Path::new(GH),
            who: WHO,
            home: Path::new("/gh-home"),
            token,
            limits: LIMITS,
        }
    }

    fn fetch(&self, github: &Github, prefix: &str) -> Result<String, FetchError> {
        self.fetcher(github, None).fetch(prefix, &self.scratch())
    }
}

fn urls(github: &Github) -> Vec<String> {
    github
        .listing_calls()
        .iter()
        .map(|c| c.args.last().unwrap().clone())
        .collect()
}

/// The digits pick the one release across the pages, an old release included: the first two
/// pages are full and hold nothing of it, the third holds it, and the listing is read until an
/// empty page and no further.
#[test]
fn the_listing_picks_the_unique_asset_across_pages() {
    let rig = Rig::new("fetch-pages");
    let github = Github::default();
    {
        let mut pages = github.pages.borrow_mut();
        pages.push(full_page(1, &[release("v9.0", &[COMMIT])]));
        pages.push(full_page(2, &[]));
        pages.push(page(&[filler(7), release("v0.1", &[OLD_COMMIT])]));
    }
    github.publish("v9.0", COMMIT);
    github.publish("v0.1", OLD_COMMIT);
    assert_eq!(
        rig.fetch(&github, "aaaaaaaaaaaa").as_deref(),
        Ok(OLD_COMMIT)
    );
    assert_eq!(
        urls(&github),
        [1, 2, 3, 4].map(|n| format!(
            "https://api.github.com/repos/komira-ai/kbf/releases?per_page=100&page={n}"
        ))
    );
    assert_eq!(github.download_calls()[0].args[2], "v0.1");
    for name in [tarball(OLD_COMMIT), bundle(OLD_COMMIT)] {
        assert!(rig.scratch().join(name).is_file());
    }

    // The release on the first page is picked the same way.
    let github = Github::default();
    github
        .pages
        .borrow_mut()
        .push(full_page(1, &[release("v9.0", &[COMMIT])]));
    github.pages.borrow_mut().push(page(&[filler(7)]));
    github.publish("v9.0", COMMIT);
    assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb").as_deref(), Ok(COMMIT));
    assert_eq!(github.listing_calls().len(), 3);
}

/// A page with fewer than 100 releases is not the end of the listing: a second commit that
/// starts with the same digits on the page after it still makes the release ambiguous.
#[test]
fn a_short_page_does_not_end_the_listing() {
    let rig = Rig::new("fetch-short");
    let github = Github::with_release();
    github
        .pages
        .borrow_mut()
        .push(page(&[release("v2.0", &[TWIN])]));
    assert_eq!(
        rig.fetch(&github, "bbbbbbbbbbbb"),
        Err(FetchError::Ambiguous)
    );
    assert!(github.download_calls().is_empty());
}

/// No asset of a commit that starts with the digits is an unknown release, however many pages
/// say so; nothing is downloaded. Assets that only look like the tarball are not it.
#[test]
fn a_version_no_asset_starts_with_is_an_unknown_release() {
    let rig = Rig::new("fetch-unknown");
    for pages in [
        Vec::new(),
        vec![page(&[filler(1)])],
        vec![full_page(1, &[]), page(&[filler(2)])],
    ] {
        let github = Github::default();
        *github.pages.borrow_mut() = pages;
        assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb"), Err(FetchError::Unknown));
        assert!(github.download_calls().is_empty());
    }
    let github = Github::default();
    let upper = COMMIT.to_uppercase();
    let names = [
        format!("{}.sig", tarball(COMMIT)),
        format!("kbf-{}-{PLATFORM}.tar.gz", &COMMIT[..39]),
        format!("kbf-{COMMIT}-darwin-arm64.tar.gz"),
        format!("kbf-installer-{COMMIT}-{PLATFORM}.tar.gz"),
        format!("Kbf-{COMMIT}-{PLATFORM}.tar.gz"),
        format!("kbf-{upper}-{PLATFORM}.tar.gz"),
        format!("kbf-{COMMIT}-{PLATFORM}.tar"),
    ];
    let assets: Vec<String> = names.iter().map(|n| asset(n, 10)).collect();
    let near = format!(
        r#"{{"tag_name":"v2.0","draft":false,"assets":[{}]}}"#,
        assets.join(",")
    );
    github.pages.borrow_mut().push(page(&[near]));
    assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb"), Err(FetchError::Unknown));
}

/// Two different commits that start with the digits are an ambiguous release, also when they are
/// on different pages; the same commit in two releases is one release, the newest.
#[test]
fn two_commits_with_the_digits_are_an_ambiguous_release() {
    let rig = Rig::new("fetch-ambiguous");
    let github = Github::default();
    github
        .pages
        .borrow_mut()
        .push(full_page(1, &[release("v3.0", &[COMMIT])]));
    github
        .pages
        .borrow_mut()
        .push(page(&[release("v2.0", &[TWIN])]));
    assert_eq!(
        rig.fetch(&github, "bbbbbbbbbbbb"),
        Err(FetchError::Ambiguous)
    );
    assert!(github.download_calls().is_empty());

    let github = Github::default();
    github.pages.borrow_mut().push(page(&[
        release("v3.0", &[COMMIT]),
        release("v2.0", &[COMMIT]),
    ]));
    github.publish("v3.0", COMMIT);
    github.publish("v2.0", COMMIT);
    assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb").as_deref(), Ok(COMMIT));
    assert_eq!(github.download_calls()[0].args[2], "v3.0");
}

/// A listing that is never exhausted is read up to the page limit and no further; with no match
/// in it that is a failed fetch (not "unknown": not all of it was seen), and a match in it is
/// taken.
#[test]
fn the_listing_is_read_up_to_the_page_limit() {
    let rig = Rig::new("fetch-limit");
    let github = Github::default();
    for n in 1..=MAX_PAGES as usize + 3 {
        github.pages.borrow_mut().push(full_page(n, &[]));
    }
    assert_eq!(
        rig.fetch(&github, "bbbbbbbbbbbb"),
        Err(FetchError::Failed(format!(
            "the listing was cut at {MAX_PAGES} pages"
        )))
    );
    assert_eq!(github.listing_calls().len(), MAX_PAGES as usize);

    let github = Github::default();
    for n in 1..=MAX_PAGES as usize + 3 {
        let extra = if n == 4 {
            vec![release("v5.0", &[COMMIT])]
        } else {
            vec![]
        };
        github.pages.borrow_mut().push(full_page(n, &extra));
    }
    github.publish("v5.0", COMMIT);
    assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb").as_deref(), Ok(COMMIT));
    assert_eq!(github.listing_calls().len(), MAX_PAGES as usize);
}

/// The download is rebuilt from the tag and the asset names, whatever URL the listing gives; an
/// entry whose tag could be taken for an option is not used.
#[test]
fn a_url_in_the_listing_is_ignored_and_the_download_is_rebuilt() {
    let rig = Rig::new("fetch-url");
    let github = Github::with_release();
    rig.fetch(&github, "bbbbbbbbbbbb").unwrap();
    let scratch = rig.scratch().display().to_string();
    assert_eq!(
        github.download_calls()[0].args,
        [
            "release",
            "download",
            "v1.0",
            "--repo",
            "komira-ai/kbf",
            "--pattern",
            &tarball(COMMIT),
            "--pattern",
            &bundle(COMMIT),
            "--dir",
            &scratch,
        ]
    );
    for call in github.calls.borrow().iter() {
        let text = format!("{} {}", call.program, call.args.join(" "));
        assert!(!text.contains("evil.example.invalid"), "{text}");
        if call.program == CURL {
            assert!(
                call.args
                    .last()
                    .unwrap()
                    .starts_with("https://api.github.com/repos/")
            );
        } else {
            assert!(!text.contains("http"), "{text}");
        }
    }

    for tag in ["--repo", "-x", ".hidden", "v1 0", "v1.0\\nx"] {
        let github = Github::default();
        github
            .pages
            .borrow_mut()
            .push(page(&[release(tag, &[COMMIT])]));
        github.publish(tag, COMMIT);
        assert_eq!(
            rig.fetch(&github, "bbbbbbbbbbbb"),
            Err(FetchError::Unknown),
            "{tag}"
        );
        assert!(github.download_calls().is_empty(), "{tag}");
    }
}

/// The listing and the download run as the account they are given, in the home it owns; the
/// download has a time limit and a limit on what it writes, both from the limits it is given,
/// and the proxy variables are not set.
#[test]
fn the_fetch_runs_as_the_given_account_with_a_size_cap_and_a_time_limit() {
    let rig = Rig::new("fetch-account");
    let github = Github::with_release();
    rig.fetch(&github, "bbbbbbbbbbbb").unwrap();
    let list = &github.listing_calls()[0];
    let download = &github.download_calls()[0];
    assert_eq!(list.program, CURL);
    assert_eq!(download.program, GH);
    for call in [list, download] {
        assert_eq!(call.run_as, Some(WHO));
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
    assert!(env.contains(&("HOME", "/gh-home")), "{env:?}");
    assert!(env.contains(&("GH_CONFIG_DIR", "/gh-home")), "{env:?}");
    assert_eq!(list.timeout, Some(LIMITS.tool));
    assert_eq!(download.timeout, Some(LIMITS.download));
    let cap = download.write_cap.as_ref().unwrap();
    assert_eq!(cap.dir, rig.scratch());
    assert_eq!(cap.max, LIMITS.tarball_max + LIMITS.bundle_max);
}

/// A file the listing says is over the limit is refused before anything is downloaded, and the
/// bundle has its own, smaller limit.
#[test]
fn an_oversize_release_file_is_refused_before_the_download() {
    let rig = Rig::new("fetch-oversize");
    for (name, was, now) in [
        (tarball(COMMIT), 1000, LIMITS.tarball_max + 1),
        (bundle(COMMIT), 100, LIMITS.bundle_max + 1),
    ] {
        let github = Github::default();
        let big = release("v1.0", &[COMMIT]).replace(
            &format!(r#""name":"{name}","size":{was}"#),
            &format!(r#""name":"{name}","size":{now}"#),
        );
        github.pages.borrow_mut().push(page(&[big]));
        github.publish("v1.0", COMMIT);
        assert_eq!(
            rig.fetch(&github, "bbbbbbbbbbbb"),
            Err(FetchError::Failed(
                "a release file is larger than the limit".into()
            ))
        );
        assert!(github.download_calls().is_empty());
    }
}

/// A release that lists no bundle for the tarball, and a listing page far larger than any page
/// of releases, are refused as a failed fetch.
#[test]
fn a_release_without_its_bundle_and_an_absurd_page_are_refused() {
    let rig = Rig::new("fetch-nobundle");
    let github = Github::default();
    let without = release("v1.0", &[COMMIT]).replace(&bundle(COMMIT), "other.sigstore.json");
    github.pages.borrow_mut().push(page(&[without]));
    github.publish("v1.0", COMMIT);
    assert_eq!(
        rig.fetch(&github, "bbbbbbbbbbbb"),
        Err(FetchError::Failed(
            "the release lists no attestation bundle for the file".into()
        ))
    );
    assert!(github.download_calls().is_empty());

    let github = Github::default();
    github
        .pages
        .borrow_mut()
        .push(format!("[{}]", " ".repeat(MAX_PAGE_BYTES)));
    assert!(matches!(
        rig.fetch(&github, "bbbbbbbbbbbb"),
        Err(FetchError::Failed(_))
    ));
}

/// A download that outlives its limit (the runner kills it, leaving what it wrote), a listing
/// page that does, a network failure of either, an answer that is not a listing and a release
/// that lacks the file are each a failed fetch with the reason of the tool.
#[test]
fn a_failure_of_the_fetch_is_a_failed_fetch_with_the_tools_reason() {
    let rig = Rig::new("fetch-fails");
    for (broken, reason) in [
        (
            "download-timeout",
            "gh release download: ran past its limit",
        ),
        ("list-timeout", "curl: ran past its limit"),
        (
            "list-net",
            "curl: exit status 1: error connecting to api.github.com",
        ),
        ("list-junk", "the answer is not a listing of releases"),
        (
            "download-net",
            "gh release download: exit status 1: error connecting to api.github.com",
        ),
    ] {
        let github = Github::with_release();
        github.break_(broken);
        assert_eq!(
            rig.fetch(&github, "bbbbbbbbbbbb"),
            Err(FetchError::Failed(reason.into())),
            "{broken}"
        );
    }
    // The release is listed and has no such file.
    let github = Github::with_release();
    github.assets.borrow_mut().clear();
    assert_eq!(
        rig.fetch(&github, "bbbbbbbbbbbb"),
        Err(FetchError::Failed(
            "gh release download: exit status 1: release not found".into()
        ))
    );
}

/// A listing that a signal ended is not reported as "exit status None": the reason names the
/// program and the signal, so the operator can tell a kill (SIGKILL), a crash (SIGSEGV, SIGABRT)
/// and a broken pipe (SIGPIPE) apart.
#[test]
fn a_listing_ended_by_a_signal_names_the_signal() {
    let rig = Rig::new("fetch-signal");
    let github = Github::with_release();
    github.break_("list-signal");
    let Err(FetchError::Failed(why)) = rig.fetch(&github, "bbbbbbbbbbbb") else {
        panic!("not a failed fetch");
    };
    assert!(why.starts_with("curl: signal 13 (SIGPIPE): "), "{why}");
    assert!(!why.contains("None"), "{why}");
}

/// With no token the listing is the system `curl`: the arguments are exactly these (`-q` first
/// so no configuration file is read, HTTPS only, no redirect, the status and headers printed with
/// the body, a time and a size limit, the fixed URL last), the command carries no environment of
/// its own, nothing is on its standard input, and nothing like a token is anywhere in the call.
#[test]
fn the_listing_runs_curl_with_no_token_and_fixed_flags() {
    let rig = Rig::new("fetch-curl");
    let github = Github::with_release();
    rig.fetch(&github, "bbbbbbbbbbbb").unwrap();
    let calls = github.listing_calls();
    assert_eq!(calls.len(), 2);
    for (i, call) in calls.iter().enumerate() {
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
                &LIMITS.tool.as_secs().to_string(),
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
        assert_eq!(call.stdout_max, Some(MAX_PAGE_BYTES));
        assert!(call.write_cap.is_none());
        let text = format!("{call:?}").to_lowercase();
        assert!(
            !text.contains("token") && !text.contains("bearer"),
            "{text}"
        );
    }
}

/// Every answer of GitHub that is not a listing is a failed fetch with a reason that says why.
/// An exhausted rate limit says when to retry and that a `token_file` raises the limit, whether
/// the API shows it by `x-ratelimit-remaining: 0`, by `retry-after` or by its message alone; a
/// 403 that is not a limit is an HTTP error like a 500. The first page is the only one asked.
#[test]
fn an_http_error_or_a_rate_limit_is_a_failed_fetch() {
    let rig = Rig::new("fetch-http");
    let failed = |why: &str| Err(FetchError::Failed(why.to_owned()));
    let limited = |why: &str| Err(FetchError::RateLimited(why.to_owned()));
    let cases = [
        (
            "list-http",
            failed("GitHub answered HTTP 500: Server Error"),
        ),
        (
            "list-rate",
            limited("rate limited by GitHub, retry after 2026-09-21T14:13:20Z or set token_file"),
        ),
        (
            "list-rate-wait",
            limited("rate limited by GitHub, retry after 60 seconds or set token_file"),
        ),
        (
            "list-rate-message",
            limited("rate limited by GitHub, retry after a while or set token_file"),
        ),
        (
            "list-forbidden",
            failed("GitHub answered HTTP 403: Forbidden"),
        ),
        ("list-nohttp", failed("the answer is not an HTTP response")),
    ];
    for (broken, want) in cases {
        let github = Github::with_release();
        github.break_(broken);
        assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb"), want, "{broken}");
        assert_eq!(github.listing_calls().len(), 1, "{broken}");
        assert!(github.download_calls().is_empty(), "{broken}");
    }
}

/// The pure reading of what `curl --include` printed: the body of a 200 (after interim answers,
/// with header names in any case), and the reason for everything else.
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
    let rig = Rig::new("fetch-interim");
    let github = Github::with_release();
    github.break_("list-interim");
    assert_eq!(rig.fetch(&github, "bbbbbbbbbbbb").as_deref(), Ok(COMMIT));
}

/// The token of a channel reaches the listing through curl's standard input only (never an
/// argument or the environment) and the download as `GH_TOKEN`, as a long workflow token does.
#[test]
fn the_token_goes_to_the_listing_by_stdin_and_the_download_only() {
    let rig = Rig::new("fetch-token");
    let long = format!(
        "ghs_123456_{}.{}.{}",
        "aB3-_".repeat(30),
        "xY9".repeat(60),
        "z_".repeat(60)
    );
    for token in ["ghp_exampleToken-0", long.as_str()] {
        let github = Github::with_release();
        rig.fetcher(&github, Some(token))
            .fetch("bbbbbbbbbbbb", &rig.scratch())
            .unwrap();
        for list in github.listing_calls() {
            assert_eq!(
                String::from_utf8(list.stdin.clone()).unwrap(),
                format!("header = \"Authorization: Bearer {token}\"\n")
            );
            assert_eq!(&list.args[list.args.len() - 4..][..2], ["--config", "-"]);
            assert!(list.env.is_empty(), "{:?}", list.env);
        }
        assert!(
            github.download_calls()[0]
                .env
                .iter()
                .any(|(k, v)| k == "GH_TOKEN" && v == token)
        );
        for call in github.calls.borrow().iter() {
            assert!(!call.args.iter().any(|a| a.contains(token)), "{call:?}");
        }
    }
}
