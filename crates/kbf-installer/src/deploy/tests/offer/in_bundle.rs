//! `deploy --offer -`: the offer is the bundle's own `offer.json`, and an offer that claims
//! `reversible` is checked against the build this host runs. Each test names the defect it
//! catches and the mutant that turned it red. As in the parent module, every refusal also
//! proves the farm was not touched.

use super::*;

/// What `--offer -` makes of the offer rules on a gamma host, with the default age limit.
fn in_bundle() -> OfferPolicy {
    OfferPolicy::InBundle {
        stage: Stage::Gamma,
        max_age: Duration::from_secs(72 * HOUR),
    }
}

/// The bytes `offer.json` holds for `fields`.
fn offer_of(fields: &Fields) -> Vec<u8> {
    fields.doc().to_bytes().expect("a valid offer")
}

/// The good release bundle, with `offer` as its `offer.json` entry (none: left out).
fn bundle_carrying(offer: Option<&[u8]>) -> Vec<u8> {
    let list = files();
    let sums = sums_of(&list);
    let mut all: Vec<(&str, &[u8])> = list
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    all.push(("SHA256SUMS", sums.as_slice()));
    if let Some(offer) = offer {
        all.push(("offer.json", offer));
    }
    tarball(&all)
}

/// The two ways to give the same offer, each with the bundle that goes with it: a file's (the
/// policy holds the document) and the bundle's own.
fn both_ways(fields: &Fields) -> [(OfferPolicy, Vec<u8>); 2] {
    let by_file = (policy(Stage::Gamma, fields.doc()), good_bundle());
    let by_bundle = (in_bundle(), bundle_carrying(Some(&offer_of(fields))));
    [by_file, by_bundle]
}

/// A probe that says what the staged `kbf` and the running one say, and counts how often the
/// running one was asked.
struct Both {
    staged: Option<u64>,
    running: Result<Option<u64>, ProbeFault>,
    asked: std::cell::Cell<usize>,
}

impl Both {
    fn new(staged: Option<u64>, running: Result<Option<u64>, ProbeFault>) -> Both {
        Both {
            staged,
            running,
            asked: std::cell::Cell::new(0),
        }
    }
}

impl Probe for Both {
    fn meta_log(
        &self,
        _: &CommitId,
        _: &[u8],
        _: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        Ok(self.staged)
    }

    fn running_meta_log(&self) -> Result<Option<u64>, ProbeFault> {
        self.asked.set(self.asked.get() + 1);
        self.running.clone()
    }
}

/// A probe that fails the test if the running build is asked at all.
struct NeverRunning(Option<u64>);

impl Probe for NeverRunning {
    fn meta_log(
        &self,
        _: &CommitId,
        _: &[u8],
        _: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        Ok(self.0)
    }

    fn running_meta_log(&self) -> Result<Option<u64>, ProbeFault> {
        panic!("the running build was asked about an offer that claims no reversibility")
    }
}

/// The text of a probe's failure; anything else fails the test.
fn failed(answer: Result<Option<u64>, ProbeFault>) -> String {
    match answer {
        Err(ProbeFault::Failed(why)) => why,
        other => panic!("not a failure: {other:?}"),
    }
}

/// Proves an offer in the bundle (`--offer -`) lets the deploy through as a file's does and is
/// recorded: the host confirms the claim of `reversible` against the build it runs and says
/// so in the summary (`host_reversible`, in the JSON line too), and `offer.json` is read, not
/// uploaded to the store (two PUTs: the two tarballs). Catches an offer never read from the
/// bundle, an entry uploaded as a release file, and a confirmation not recorded.
///
/// Mutants to plant: `offer.json` kept among the files to upload; `host_reversible` not set;
/// the record of the bundle's offer dropped.
#[test]
fn an_offer_in_the_bundle_lets_the_deploy_through_and_is_recorded() {
    let probe = Both::new(Some(4), Ok(Some(5)));
    let offer = offer_of(&Fields::good());
    let bundle = bundle_carrying(Some(&offer));
    let (f, s) = deploy_probing(in_bundle(), Some(bundle), &probe);
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(f.requests("PUT"), 2, "{}", s.line());
    let names: Vec<_> = s.steps.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names[..2], ["offer", "bundle"]);
    let rec = s.offer.as_ref().expect("the offer is recorded");
    assert_eq!((rec.kind, rec.stage.as_str()), ("offer", "gamma"));
    assert_eq!(rec.commit.as_deref(), Some(TARGET));
    assert_eq!(rec.reversible, Some(true));
    assert_eq!(rec.host_reversible, Some(true));
    assert_eq!(probe.asked.get(), 1);
    let line = s.line();
    assert!(line.contains("\"host_reversible\":true"), "{line}");
}

/// Proves an offer that claims no reversibility is deployed without asking the host, whether
/// it is a file's or the bundle's: the probe of the running build panics if it is called, and
/// the summary has the offer's own `reversible: false` and no `host_reversible`. Catches an
/// irreversible release refused or held (it deploys, recorded as irreversible), and a host
/// that reads its running build whatever the offer claims.
///
/// Mutants to plant: `check_reversible` refusing an offer with `reversible: false`; the
/// early return for `reversible: false` removed (the probe is asked).
#[test]
fn an_irreversible_offer_is_deployed_and_the_host_is_not_asked() {
    let mut fields = Fields::good();
    fields.meta_log = Some(6);
    for (under, bundle) in both_ways(&fields) {
        let (f, s) = deploy_probing(under, Some(bundle), &NeverRunning(Some(6)));
        assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
        assert_eq!(f.requests("PUT"), 2, "{}", s.line());
        let rec = s.offer.as_ref().expect("the offer is recorded");
        assert_eq!(rec.reversible, Some(false));
        assert_eq!(rec.host_reversible, None);
        let line = s.line();
        assert!(!line.contains("host_reversible"), "{line}");
    }
}

/// Proves a claim of `reversible` is refused (`offer_reversible_overclaim`, exit 3, the farm
/// untouched) unless the build this host runs has a `meta_log` at least the offer's, for a
/// file's offer and the bundle's alike. The offer's own numbers (4 against a
/// `previous_meta_log` of 5) are not what is checked: running 3 is below, running none is
/// unknown, and a probe that cannot say (either fault) is unknown too. Running 4 (equal) and 9
/// pass. Catches the check skipped, unknown or an error read as reversible, and the boundary
/// off by one.
///
/// Mutants to plant: `check_reversible` not called; `Ok(None)` read as reversible; `<` for
/// `<=`; a probe error read as reversible.
#[test]
fn a_claim_of_reversible_the_running_build_does_not_bear_out_is_refused() {
    let cases: [(Result<Option<u64>, ProbeFault>, bool); 6] = [
        (Ok(Some(3)), false),
        (Ok(None), false),
        (Err(ProbeFault::Failed("boom".into())), false),
        (Err(ProbeFault::Unattested("boom".into())), false),
        (Ok(Some(4)), true),
        (Ok(Some(9)), true),
    ];
    for (running, bears_out) in cases {
        for (under, bundle) in both_ways(&Fields::good()) {
            let probe = Both::new(Some(4), running.clone());
            let (f, s) = deploy_probing(under, Some(bundle), &probe);
            let held = s.offer.as_ref().and_then(|o| o.host_reversible);
            if bears_out {
                assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
                assert_eq!(held, Some(true), "{}", s.line());
            } else {
                refused(&f, &s, "offer_reversible_overclaim");
                assert_eq!(held, None, "{}", s.line());
            }
            assert_eq!(probe.asked.get(), 1);
        }
    }
}

/// Proves an offer in the bundle is judged as a file's is: for another commit, for the other
/// stage, too old and from the future each gives the reason a file's offer gives, with the
/// farm untouched. Catches the header checks skipped for the bundle's offer.
///
/// Mutant to plant: `check_header` not called for the offer read from the bundle.
#[test]
fn an_offer_in_the_bundle_is_judged_as_a_files_is() {
    let mut other_commit = Fields::good();
    other_commit.commit = OLD;
    let mut other_stage = Fields::good();
    other_stage.target = "prod";
    let mut stale = Fields::good();
    stale.created = NOW - 72 * HOUR - 1;
    let mut future = Fields::good();
    future.created = NOW + 301;
    let cases = [
        (other_commit, "offer_wrong_commit"),
        (other_stage, "offer_wrong_stage"),
        (stale, "offer_stale"),
        (future, "offer_future"),
    ];
    for (fields, reason) in cases {
        for (under, bundle) in both_ways(&fields) {
            let probe = Both::new(Some(4), Ok(Some(5)));
            let (f, s) = deploy_probing(under, Some(bundle), &probe);
            refused(&f, &s, reason);
            assert!(s.offer.is_some(), "{reason}: {}", s.line());
        }
    }
}

/// Proves the bundle must carry an offer that reads: no `offer.json` and no bundle at all are
/// `offer_missing`, and a file that is not JSON, one with a field the offer does not have and
/// one whose `reversible` the numbers do not give are `offer_unreadable`. The refusal never
/// repeats the entry, and the host is not asked. Catches an offer made up when the entry is
/// absent, a looser parse than a file's, and an unreadable offer echoed in the summary.
///
/// Mutants to plant: the missing entry read as a default offer; the unknown field accepted; the
/// parse error shown whole; the bundle's offer read from the file path `-`.
#[test]
fn the_bundle_must_carry_a_readable_offer() {
    let probe = Both::new(Some(4), Ok(Some(5)));
    let run = |bundle: Option<Vec<u8>>| deploy_probing(in_bundle(), bundle, &probe);
    for bundle in [Some(bundle_carrying(None)), None] {
        let (f, s) = run(bundle);
        refused(&f, &s, "offer_missing");
        assert!(s.offer.is_none(), "{}", s.line());
    }
    let secret = "SECRET-MARKER-9";
    let good = String::from_utf8(offer_of(&Fields::good())).unwrap();
    let unknown = good.replacen('{', &format!("{{\"{secret}\": 1,"), 1);
    let edited = good.replace("\"reversible\": true", "\"reversible\": false");
    assert_ne!(edited, good);
    for junk in ["{ not json".to_owned(), unknown, edited.clone()] {
        let (f, s) = run(Some(bundle_carrying(Some(junk.as_bytes()))));
        refused(&f, &s, "offer_unreadable");
        assert!(!s.line().contains(secret), "{}", s.line());
    }
    // The offer's own refusals are named, as a file's are.
    let (_, s) = run(Some(bundle_carrying(Some(edited.as_bytes()))));
    assert!(s.detail.contains("reversible"), "{}", s.line());
    assert_eq!(probe.asked.get(), 0);
}

/// Proves a plan (no `--apply`) with an offer in the bundle runs the checks an apply runs: the
/// good bundle plans and records the host's confirmation, and a claim the running build does
/// not bear out and an offer for another commit are each refused, with nothing uploaded and no
/// update asked for. Catches the offer or the reversibility check run only when applying.
///
/// Mutant to plant: the checks of the bundle's offer made `apply`-only.
#[test]
fn a_plan_with_an_offer_in_the_bundle_runs_the_same_checks() {
    let plan = |fields: &Fields, running: Option<u64>| {
        let bundle = bundle_carrying(Some(&offer_of(fields)));
        let probe = Both::new(Some(4), Ok(running));
        deploy_mode(false, in_bundle(), Some(bundle), &probe)
    };
    let (f, s) = plan(&Fields::good(), Some(5));
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!((f.requests("PUT"), f.requests("POST")), (0, 0));
    assert_eq!(s.offer.unwrap().host_reversible, Some(true));
    let (f, s) = plan(&Fields::good(), Some(3));
    refused(&f, &s, "offer_reversible_overclaim");
    let mut other = Fields::good();
    other.commit = OLD;
    let (f, s) = plan(&other, Some(5));
    refused(&f, &s, "offer_wrong_commit");
}

/// Proves the platforms an offer in the bundle found complete are checked against the farm's,
/// as a file's are. Catches the preflight reading the platforms of a file's offer only.
///
/// Mutant to plant: the preflight's platform check reading the policy's document alone.
#[test]
fn the_platforms_of_an_offer_in_the_bundle_are_checked_against_the_farm() {
    let mut fields = Fields::good();
    fields.platforms = vec!["linux-x86_64"];
    for (under, bundle) in both_ways(&fields) {
        let probe = Both::new(Some(4), Ok(Some(5)));
        let (f, s) = deploy_probing(under, Some(bundle), &probe);
        assert_eq!(
            (s.exit, s.reason.as_str()),
            (3, "offer_platform"),
            "{}",
            s.line()
        );
        assert_eq!(f.requests("POST"), 0);
    }
}

/// Proves the bundle reader takes exactly one `offer.json`, non-empty and of at most
/// `MAX_OFFER` bytes, keeps it out of the files to upload, and refuses a second, an empty one,
/// one byte over the cap, and names that only look like it. Catches a last entry winning, the
/// cap missing or off by one, and a lookalike taken for the offer.
///
/// Mutants to plant: a second `offer.json` accepted; the empty file accepted; the cap removed;
/// the cap `>=`; the match on the name made case-insensitive or a prefix.
#[test]
fn the_bundle_reads_one_bounded_offer_entry() {
    let commit = c(TARGET);
    let tar_name = tar_name("kbf", "linux-x86_64");
    let with = |extra: &[(&str, &[u8])]| {
        let mut files: Vec<(&str, &[u8])> = vec![(tar_name.as_str(), b"t")];
        files.extend_from_slice(extra);
        tarball(&files)
    };
    let read = |bytes: &[u8]| bundle::read_bundle(bytes, &commit);
    let one = read(&with(&[("offer.json", b"offer")])).unwrap();
    assert_eq!(one.offer.as_deref(), Some(&b"offer"[..]));
    assert_eq!(one.entries.len(), 1);
    let none = read(&with(&[])).unwrap();
    assert_eq!(none.offer, None);
    let twice = with(&[("offer.json", b"one"), ("offer.json", b"two")]);
    assert!(read(&twice).is_err());
    let empty = with(&[("offer.json", b"")]);
    assert!(read(&empty).is_err());
    let cap = vec![b'x'; offer::MAX_OFFER as usize];
    let at_cap = with(&[("offer.json", cap.as_slice())]);
    assert!(read(&at_cap).is_ok());
    let over = vec![b'x'; offer::MAX_OFFER as usize + 1];
    let over_cap = with(&[("offer.json", over.as_slice())]);
    assert!(read(&over_cap).is_err());
    for name in ["offer.json.attestation", "Offer.json", "offer.json.bak"] {
        let lookalike = with(&[(name, b"offer")]);
        assert!(read(&lookalike).is_err(), "{name}");
    }
}

/// Proves the running build is asked the way the staged one is: the program is the `kbf` the
/// probe names, started with `--version --json`, an empty environment, `/` as its directory
/// and the unprivileged user; its `meta_log` is returned, and none when it names none. A
/// non-zero exit, a timeout, output that is not a version and a probe that names no program
/// are each a failure, never a number. Catches a constant answer, the run as root, a leaked
/// environment, and an error read as "names none".
///
/// Mutants to plant: `run_as` not passed on; the exit code and the timeout not checked; the
/// parse error read as no `meta_log`.
#[test]
fn the_running_kbf_is_asked_as_the_user_and_answers_only_with_what_it_prints() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-running");
    let running = dir.join("current").join("kbf");
    let short = &TARGET[..12];
    let version = |meta: &str| {
        format!(
            "{{\"commit\":\"{short}\",{meta}\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"state_schema\":1,\"version\":\"0.1.0\"}}"
        )
    };
    // SAFETY: geteuid and getegid have no preconditions and cannot fail.
    let me = crate::run::RunAs {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    let ask = |stdout: String, code: i32, timed_out: bool| {
        let plays = Arc::new(Plays {
            stdout,
            code,
            timed_out,
            asked: Default::default(),
        });
        let probe = HostProbe::new(
            dir.join("work"),
            Some(me),
            Box::new(plays.clone()),
            Box::new(Vouches::new(Ok(()))),
        );
        let probe = probe.with_running(running.clone());
        (probe.running_meta_log(), plays)
    };
    let numbered = version("\"meta_log\":4,");
    let (answer, plays) = ask(numbered.clone(), 0, false);
    assert_eq!(answer, Ok(Some(4)));
    {
        let asked = plays.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].program, running);
        assert_eq!(asked[0].args, ["--version", "--json"]);
        assert!(asked[0].env.is_empty());
        assert_eq!(asked[0].run_as, Some(me));
        assert_eq!(asked[0].cwd, std::path::PathBuf::from("/"));
    }
    let (unnumbered, _) = ask(version(""), 0, false);
    assert_eq!(unnumbered, Ok(None));
    let (answer, _) = ask(numbered.clone(), 3, false);
    let ended = failed(answer);
    assert!(ended.contains("ended Some(3)"), "{ended}");
    let (answer, _) = ask(numbered.clone(), 0, true);
    let timed_out = failed(answer);
    assert!(timed_out.contains("ended"), "{timed_out}");
    let (answer, _) = ask("not json".into(), 0, false);
    let garbled = failed(answer);
    assert!(garbled.starts_with("the running kbf failed:"), "{garbled}");
    let nothing = Arc::new(Plays {
        stdout: numbered,
        code: 0,
        timed_out: false,
        asked: Default::default(),
    });
    let bare = HostProbe::new(
        dir.join("work"),
        None,
        Box::new(nothing),
        Box::new(Vouches::new(Ok(()))),
    );
    let unnamed = failed(bare.running_meta_log());
    assert!(unnamed.contains("names none"), "{unnamed}");
    assert!(NoProbe.running_meta_log().is_err());
}

/// Proves the probe of a real host runs the `kbf` of the `current` release under its root:
/// with no release the program is not found, and with a file there that cannot be run the
/// same path is found and refused. A failure either way, never an answer. Catches a probe
/// that is pointed anywhere else, and a missing release read as "names none".
///
/// Mutant to plant: the path taken from the `previous` link instead of `current`.
#[test]
fn the_host_probe_runs_the_kbf_of_the_current_release() {
    use crate::layout::{Layout, Platform};

    let (_, dir) = crate::testkit::host_root("deploy-offer-current");
    let layout = Layout::new(kbf_host::Root::new(&dir).unwrap(), Platform::current());
    let probe = HostProbe::at(&dir, false).unwrap();
    let missing = failed(probe.running_meta_log());
    assert!(missing.contains("os error 2"), "{missing}");
    let binary = layout.join(&layout.current()).join("kbf");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(&binary, b"not a program").unwrap();
    let not_run = failed(probe.running_meta_log());
    assert!(not_run.contains("os error 13"), "{not_run}");
}
