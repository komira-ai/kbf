//! What an action run as one pool user reaches of another pool user, measured and
//! printed, not asserted (T026, step M0): the Seatbelt profile of a slot (`src/network.rs`)
//! limits writes and little else, so this records, for every kind of reach, what the
//! sandbox lets through and what the kernel's modes and uids already stop.
//!
//! Two pool users, A and B, each the only user of a runtime of its own (so A is always
//! `users.0[0]`). Lease A starts first and stays up, holding what B will try to reach:
//! a TCP listener on every address, Unix sockets in its lease directory and in its
//! temporary folder, and a marker process with the marker in its arguments. Root plants
//! files owned by A in A's home, A's temporary folder (`getconf DARWIN_USER_TEMP_DIR`),
//! A's lease directory, and the shared places (`/private/tmp`, `/private/var/tmp`, the
//! `Shared` folder under `/Users`, `/Library/Caches`): one mode 0644 file to read and one
//! mode 0666 file to write in each. (A cannot make the shared ones: its writes there
//! are refused.)
//!
//! Then B tries each reach, in one `kbf-act` run of probe steps (`kbf_testkit::act`,
//! one `PROBE` line per step), as:
//! - a sandboxed lease with the network on (the default) and
//! - one with `network=off`, both through the driver (`launchctl asuser`, `__as-user`,
//!   `sandbox-exec` with the slot profile), and
//! - the control: the same steps through the same `launchctl asuser` and `__as-user`,
//!   with no `sandbox-exec`.
//!
//! The file rows run twice: `as-is`, with the modes the system gave, and `opened`, with
//! A's home, temporary folder, lease directory and the folders above them mode 0755, so
//! that only the sandbox is left to refuse. A row whose control is refused, where the
//! control should succeed, proves nothing when the sandboxed cell is refused too; the
//! test prints a `NOTE` line for it.
//!
//! Output, to stdout (run with `--nocapture`): `FIXTURE` lines (modes and owners of what
//! the rows reach), `REACH <row> <network> <sandboxed|control> <ok|errno=N>` for every
//! cell, `NOTE` lines, and a table of the same cells.
//!
//! **Asserted:** only that the harness worked: the users and their sandboxes started,
//! A's listeners and marker are up and A ran as its user, every fixture was planted,
//! each B run exited 0 with one `PROBE` line per row in order, and A's lease ended 0.
//! Nothing about the profile: no cell can turn this test red, since it is a
//! measurement (a later step turns the rows it wants into assertions). A run that cannot
//! set up fails, and says what could not be set up.
//!
//! Every test is `#[ignore]`: it needs root and real users. The macOS CI job runs it with
//! `cargo xtask sudo-test -p kbf-driver-native --test slot_reach -- --test-threads=1
//! --nocapture`, after `tests/slots.rs` and `tests/slot_clean.rs` (users from uid 690,
//! made at free uids and deleted when the test ends, as theirs; the tests run one at a
//! time).

#![cfg(target_os = "macos")]

mod support;

use std::collections::BTreeMap;
use std::net::{IpAddr, UdpSocket};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::Runtime as _;
use kbf_daemon::uid::LAUNCHCTL;
use kbf_driver_native::NativeRuntime;
use kbf_driver_native::slots::{AS_USER_ARG, Pool, SlotUser};
use support::pool::{GID, Scratch, Users, config, home_of, slot_report};
use support::{MemoryCas, PROMPT, Spec, stderr, stdout};

fn assert_root() {
    support::pool::assert_root("slot_reach");
}

/// How the file rows are run: with the modes the system gave, or with A's directories
/// opened to everyone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    AsIs,
    Opened,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Self::AsIs => "as-is",
            Self::Opened => "opened",
        }
    }
}

/// One thing B tries: its name in the output, the act step, and whether the control is
/// expected to be allowed (so that a refusal of the sandboxed cell means something).
struct Row {
    id: String,
    step: Vec<String>,
    control_should_pass: bool,
}

fn row(id: String, step: &[&str], control_should_pass: bool) -> Row {
    Row {
        id,
        step: step.iter().map(|w| (*w).to_owned()).collect(),
        control_should_pass,
    }
}

/// A directory where the test plants a file to read (mode 0644) and one to write (mode
/// 0666), the file names carrying this process's id so that no run meets another's.
struct Place {
    label: &'static str,
    dir: PathBuf,
    /// What `try-list` opens.
    listed: PathBuf,
}

impl Place {
    fn new(label: &'static str, dir: PathBuf) -> Self {
        Self {
            label,
            listed: dir.clone(),
            dir,
        }
    }

    fn file(&self, kind: &str) -> String {
        self.dir
            .join(format!("kbf-reach-{}-{kind}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    /// The rows that reach this place: read, list and write.
    fn rows(&self, suffix: &str, control_should_pass: bool) -> Vec<Row> {
        let label = self.label;
        vec![
            row(
                format!("read.{label}{suffix}"),
                &["try-read", &self.file("r")],
                control_should_pass,
            ),
            row(
                format!("list.{label}{suffix}"),
                &["try-list", &self.listed.to_string_lossy()],
                control_should_pass,
            ),
            row(
                format!("write.{label}{suffix}"),
                &["try-write", &self.file("w")],
                control_should_pass,
            ),
        ]
    }
}

/// The files the test planted outside any directory a lease removes, taken away when the
/// test ends, or before.
#[derive(Default)]
struct Planted(Vec<PathBuf>);

impl Planted {
    /// Makes `place`'s two files, owned by `owner`.
    fn place(&mut self, place: &Place, owner: u32) {
        for (kind, mode) in [("r", 0o644), ("w", 0o666)] {
            let path = PathBuf::from(place.file(kind));
            std::fs::write(&path, b"kbf-reach\n")
                .unwrap_or_else(|e| panic!("cannot plant {}: {e}", path.display()));
            std::os::unix::fs::chown(&path, Some(owner), Some(GID)).expect("chown");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            self.0.push(path);
        }
    }

    fn remove(&mut self) {
        for path in self.0.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        self.remove();
    }
}

/// `args` run as `user` outside any sandbox, the way the driver starts an action up to
/// the sandbox: `launchctl asuser <uid> <as-user binary> __as-user <uid> <gid> -- args`.
#[derive(Clone)]
struct Control {
    as_user: String,
    user: SlotUser,
}

impl Control {
    fn new(scratch: &Scratch, user: &SlotUser) -> Self {
        Self {
            as_user: scratch.as_user_binary(),
            user: user.clone(),
        }
    }

    fn run(&self, args: &[String]) -> Output {
        let (uid, gid) = (self.user.uid.to_string(), self.user.gid.to_string());
        Command::new(LAUNCHCTL)
            .args(["asuser", &uid, &self.as_user, AS_USER_ARG, &uid, &gid, "--"])
            .args(args)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .output()
            .expect(LAUNCHCTL)
    }

    /// The user's temporary folder, `getconf DARWIN_USER_TEMP_DIR` as the user (which
    /// makes it when it is missing), every link resolved.
    fn temp_dir(&self) -> PathBuf {
        let out = self.run(&[
            "/usr/bin/getconf".to_owned(),
            "DARWIN_USER_TEMP_DIR".to_owned(),
        ]);
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && !said.trim().is_empty(),
            "getconf DARWIN_USER_TEMP_DIR as {}: {out:?}",
            self.user.name
        );
        std::fs::canonicalize(said.trim()).unwrap_or_else(|e| {
            panic!(
                "the temporary folder {:?} of {}: {e}",
                said.trim(),
                self.user.name
            )
        })
    }
}

/// A runtime whose pool is `user` alone, over a lease directory of its own.
fn runtime(
    scratch: &Scratch,
    leases: &Path,
    user: &SlotUser,
    cas: &Arc<MemoryCas>,
) -> NativeRuntime<MemoryCas> {
    let pool = Pool::new(vec![user.clone()]).expect("pool");
    let mut config = config(scratch, pool);
    config.scratch = leases.to_owned();
    NativeRuntime::new(config, Arc::clone(cas)).expect("runtime")
}

/// The act steps, joined by `;`, after the program.
struct Steps(Vec<String>);

impl Steps {
    fn new(act: &str) -> Self {
        Self(vec![act.to_owned()])
    }

    fn step(&mut self, words: &[&str]) {
        if self.0.len() > 1 {
            self.0.push(";".to_owned());
        }
        self.0.extend(words.iter().map(|w| (*w).to_owned()));
    }

    fn words(&self) -> Vec<&str> {
        self.0.iter().map(String::as_str).collect()
    }
}

/// Waits up to a half minute for `path` to exist.
async fn appears(path: &Path, users: &[SlotUser], what: &str) {
    let give_up = Instant::now() + PROMPT * 3;
    while !path.exists() {
        assert!(
            Instant::now() < give_up,
            "cannot set up: {what}: {} never appeared\n{}",
            path.display(),
            slot_report(users)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The address of this host that is not loopback, as the route to a documentation
/// address picks it (a datagram socket connected there sends nothing).
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// A line of the fixture report: the mode and owner of `path`.
fn fixture(label: &str, path: &Path) {
    match std::fs::metadata(path) {
        Ok(m) => println!(
            "FIXTURE {label} {} mode={:o} uid={}",
            path.display(),
            m.permissions().mode() & 0o7777,
            m.uid()
        ),
        Err(e) => println!("FIXTURE {label} {} unreadable: {e}", path.display()),
    }
}

/// What A holds and where B looks for it.
struct Facts {
    /// The places A owns: its files are reached by their modes and the sandbox.
    owned: Vec<Place>,
    /// The places every user may use.
    shared: Vec<Place>,
    port: u16,
    lan: Option<IpAddr>,
    marker: String,
    marker_pid: String,
    sock_lease: String,
    sock_tmp: String,
    b_home: PathBuf,
    b_tmp: PathBuf,
}

/// The rows of one run, and the steps that precede them (making what the rows of B's own
/// lease directory need).
fn rows(f: &Facts, variant: Variant, own_lease: &Path) -> (Vec<Vec<String>>, Vec<Row>) {
    let opened = variant == Variant::Opened;
    let suffix = format!(".{}", variant.name());
    let mut rows = Vec::new();
    for place in &f.owned {
        rows.extend(place.rows(&suffix, opened));
    }
    rows.push(row(
        format!("unix.a-lease{suffix}"),
        &["unix-connect", &f.sock_lease],
        opened,
    ));
    rows.push(row(
        format!("unix.a-tmpdir{suffix}"),
        &["unix-connect", &f.sock_tmp],
        opened,
    ));
    let mut before = Vec::new();
    if variant == Variant::AsIs {
        for place in &f.shared {
            rows.extend(place.rows("", true));
        }
        let loopback = format!("127.0.0.1:{}", f.port);
        rows.push(row(
            "tcp.loopback".to_owned(),
            &["tcp-connect", &loopback],
            true,
        ));
        if let Some(lan) = f.lan {
            let lan = format!("{lan}:{}", f.port);
            rows.push(row("tcp.lan".to_owned(), &["tcp-connect", &lan], true));
        }
        rows.push(row("ps.marker".to_owned(), &["ps-sees", &f.marker], false));
        rows.push(row(
            "procargs.marker".to_owned(),
            &["procargs", &f.marker_pid],
            false,
        ));
        rows.push(row(
            "proc-sees.marker".to_owned(),
            &["proc-sees", &f.marker_pid],
            false,
        ));
        // B's own: made by root where B owns the place, by B's run where it is the lease.
        let own_lease = Place::new("b-lease", own_lease.to_owned());
        for file in ["r", "w"] {
            before.push(vec![
                "write-lines".to_owned(),
                own_lease.file(file),
                "x".to_owned(),
            ]);
        }
        let own = [
            Place::new("b-home", f.b_home.clone()),
            Place::new("b-tmpdir", f.b_tmp.clone()),
            own_lease,
        ];
        for place in own {
            let label = place.label;
            let [read, list, write] = place.rows("", true).try_into().ok().expect("three rows");
            rows.push(Row {
                id: format!("own-read.{label}"),
                ..read
            });
            rows.push(Row {
                id: format!("own-list.{label}"),
                ..list
            });
            rows.push(Row {
                id: format!("own-write.{label}"),
                ..write
            });
        }
    }
    (before, rows)
}

/// The argv of a run: the act, the steps before the rows, then the rows' steps.
fn argv(act: &str, before: &[Vec<String>], rows: &[Row]) -> Vec<String> {
    let mut steps = Steps::new(act);
    for step in before.iter().chain(rows.iter().map(|r| &r.step)) {
        let words: Vec<&str> = step.iter().map(String::as_str).collect();
        steps.step(&words);
    }
    steps.0
}

/// What each row came to, in order, from the `PROBE` lines of `out`; an error that says
/// what is wrong when the lines are not the rows'.
fn results(out: &str, rows: &[Row]) -> Result<Vec<String>, String> {
    let lines: Vec<Vec<&str>> = out
        .lines()
        .filter(|l| l.starts_with("PROBE "))
        .map(|l| l.split_whitespace().collect())
        .collect();
    if lines.len() != rows.len() {
        return Err(format!(
            "{} PROBE lines for {} rows",
            lines.len(),
            rows.len()
        ));
    }
    let mut found = Vec::new();
    for (line, row) in lines.iter().zip(rows) {
        match line.as_slice() {
            ["PROBE", name, result] if *name == row.step[0] => found.push((*result).to_owned()),
            _ => return Err(format!("row {}: the line {line:?}", row.id)),
        }
    }
    Ok(found)
}

/// Each cell of the table: the row, and the result per column.
#[derive(Default)]
struct Table {
    order: Vec<String>,
    cells: BTreeMap<String, BTreeMap<&'static str, String>>,
    should_pass: BTreeMap<String, bool>,
}

impl Table {
    fn record(&mut self, column: &'static str, rows: &[Row], found: Vec<String>) {
        for (row, result) in rows.iter().zip(found) {
            if !self.cells.contains_key(&row.id) {
                self.order.push(row.id.clone());
            }
            self.should_pass
                .insert(row.id.clone(), row.control_should_pass);
            self.cells
                .entry(row.id.clone())
                .or_default()
                .insert(column, result);
        }
    }

    fn print(&self) {
        let columns = [
            ("on", "sandboxed", "on"),
            ("off", "sandboxed", "off"),
            ("-", "control", "control"),
        ];
        for id in &self.order {
            for (network, mode, column) in columns {
                let result = self.cells[id].get(column).map_or("-", String::as_str);
                println!("REACH {id} {network} {mode} {result}");
            }
        }
        for id in &self.order {
            let control = self.cells[id].get("control").map_or("-", String::as_str);
            if self.should_pass[id] && control != "ok" {
                println!(
                    "NOTE {id} control={control}: the control should pass, so a refusal of \
                     the sandboxed cell proves nothing"
                );
            }
        }
        println!("{:<34} {:<10} {:<10} {:<10}", "row", "on", "off", "control");
        for id in &self.order {
            let get = |column: &str| self.cells[id].get(column).map_or("-", String::as_str);
            println!(
                "{id:<34} {:<10} {:<10} {:<10}",
                get("on"),
                get("off"),
                get("control")
            );
        }
    }
}

/// B's own places: a file to read and one to write in its home and its temporary folder,
/// B's. (Its lease directory is made by the run itself.)
fn plant_own(b: &SlotUser, tmp: &Path) -> Planted {
    let mut own = Planted::default();
    for place in [
        Place::new("b-home", home_of(b)),
        Place::new("b-tmpdir", tmp.to_owned()),
    ] {
        own.place(&place, b.uid);
    }
    own
}

/// The measurement. Catches nothing about the profile (module docs); fails when the
/// harness did not work.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_reach"]
#[tokio::test]
async fn what_an_action_reaches_of_another_slot_is_recorded() {
    assert_root();
    let scratch = Scratch::new("reach");
    let users = Users::new("reach", 2);
    let (a, b) = (users.0[0].clone(), users.0[1].clone());
    let cas = Arc::new(MemoryCas::default());
    let (a_leases, b_leases) = (scratch.0.join("leases-a"), scratch.0.join("leases-b"));
    let rt_a = runtime(&scratch, &a_leases, &a, &cas);
    let rt_b = runtime(&scratch, &b_leases, &b, &cas);
    let (control_a, control_b) = (Control::new(&scratch, &a), Control::new(&scratch, &b));
    let act = scratch.act();

    let sw_vers = Command::new("/usr/bin/sw_vers").output().expect("sw_vers");
    println!(
        "SW_VERS {}",
        String::from_utf8_lossy(&sw_vers.stdout).replace('\n', " ")
    );

    let a_dir = a_leases.join("lease-1-1");
    let a_root = a_dir.join("root");
    let a_tmp = control_a.temp_dir();
    let marker = format!("kbf-reach-marker-{}", std::process::id());
    let (sock_lease, sock_tmp) = (
        a_root.join("reach.sock").to_string_lossy().into_owned(),
        a_tmp.join("kbf-reach.sock").to_string_lossy().into_owned(),
    );
    let paths = |name: &str| a_root.join(name).to_string_lossy().into_owned();
    let (port, pids, ready, done) = (
        paths("port"),
        paths("marker-pid"),
        paths("ready"),
        paths("done"),
    );
    let mut hold = Steps::new(&act);
    hold.step(&["spawn-child", "tcp-listen", "0.0.0.0:0", &port, "900"]);
    hold.step(&["spawn-child", "unix-listen", &sock_lease, "900"]);
    hold.step(&["spawn-child", "unix-listen", &sock_tmp, "900"]);
    hold.step(&[
        "spawn-child",
        "--pid",
        &pids,
        "sleep",
        "300.7391",
        ";;",
        "echo",
        &marker,
    ]);
    for file in [&port, &sock_lease, &sock_tmp, &pids] {
        hold.step(&["wait-for", file, "20"]);
    }
    hold.step(&["write-lines", &ready]);
    hold.step(&["wait-for", &done, "900"]);
    let hold = Spec::argv(&hold.words()).store(&cas);
    let run_a = async {
        let work = support::work(1, hold, 0);
        tokio::time::timeout(Duration::from_secs(900), rt_a.run(work))
            .await
            .expect("lease A ends")
    };

    let orchestrate = async {
        appears(
            &a_root.join("ready"),
            &users.0,
            "lease A holding its listeners",
        )
        .await;
        let owner = std::fs::metadata(&ready).expect("ready").uid();
        assert_eq!(owner, a.uid, "lease A did not run as its pool user");
        let read = |path: &str| std::fs::read_to_string(path).expect(path);
        let a_port: u16 = read(&port).trim().parse().expect("A's port");
        let marker_pid = read(&pids).trim().to_owned();
        assert!(
            marker_pid.parse::<i32>().is_ok(),
            "A's marker pid {marker_pid:?}"
        );

        let a_home = home_of(&a);
        let mut planted = Planted::default();
        let owned = vec![
            Place::new("a-home", a_home.clone()),
            Place::new("a-tmpdir", a_tmp.clone()),
            Place {
                label: "a-lease",
                dir: a_root.clone(),
                listed: a_dir.clone(),
            },
        ];
        let shared = vec![
            Place::new("tmp", PathBuf::from("/private/tmp")),
            Place::new("var-tmp", PathBuf::from("/private/var/tmp")),
            Place::new("shared", Path::new("/Users").join("Shared")),
            Place::new("caches", PathBuf::from("/Library/Caches")),
        ];
        for place in owned.iter().chain(&shared) {
            planted.place(place, a.uid);
        }
        let lan = lan_address();
        let mut facts = Facts {
            owned,
            shared,
            port: a_port,
            lan,
            marker: marker.clone(),
            marker_pid,
            sock_lease: sock_lease.clone(),
            sock_tmp: sock_tmp.clone(),
            b_home: home_of(&b),
            b_tmp: control_b.temp_dir(),
        };
        println!("LAN {lan:?}");
        let mut table = Table::default();
        let mut seq = 0;
        let entry = a_tmp.parent().expect("the folders entry").to_owned();
        for variant in [Variant::AsIs, Variant::Opened] {
            println!("VARIANT {}", variant.name());
            if variant == Variant::Opened {
                // Every directory above a planted file that its unix mode closed, opened.
                for dir in [&a_home, &a_tmp, &entry, &a_dir, &a_root] {
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
                        .unwrap_or_else(|e| panic!("cannot open {}: {e}", dir.display()));
                }
            }
            for (label, path) in [
                ("a-home", &a_home),
                ("a-tmpdir", &a_tmp),
                ("a-folders-entry", &entry),
                ("a-lease-dir", &a_dir),
                ("a-lease-root", &a_root),
            ] {
                fixture(label, path);
            }
            for network in ["on", "off"] {
                seq += 1;
                // B's places, as its last clean left them: its home was emptied and its
                // temporary folder may be gone.
                let b_tmp = control_b.temp_dir();
                let own = plant_own(&b, &b_tmp);
                facts.b_tmp = b_tmp;
                let own_lease = b_leases.join(format!("lease-1-{seq}")).join("root");
                let (before, run_rows) = rows(&facts, variant, &own_lease);
                let argv = argv(&act, &before, &run_rows);
                let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                let mut spec = Spec::argv(&argv);
                if network == "off" {
                    spec = spec.property("network", "off");
                }
                let work = support::work(seq, spec.store(&cas), 0);
                let result = tokio::time::timeout(Duration::from_secs(180), rt_b.run(work))
                    .await
                    .expect("B's lease ends")
                    .expect("B ran");
                drop(own);
                let (out, err) = (stdout(&cas, &result), stderr(&cas, &result));
                assert_eq!(
                    result.exit_code,
                    0,
                    "B's sandboxed run ({network}, {}): {out}{err}\n{}",
                    variant.name(),
                    slot_report(&users.0)
                );
                let found = results(&out, &run_rows).unwrap_or_else(|why| {
                    panic!("B's sandboxed run ({network}): {why}\n{out}{err}")
                });
                table.record(if network == "on" { "on" } else { "off" }, &run_rows, found);
            }
            // The control, in a directory of its own for what B makes in its lease.
            let own_lease = scratch.0.join(format!("control-{}", variant.name()));
            std::fs::create_dir_all(&own_lease).expect("control directory");
            std::os::unix::fs::chown(&own_lease, Some(b.uid), Some(GID)).expect("chown");
            std::fs::set_permissions(&own_lease, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
            let b_tmp = control_b.temp_dir();
            let own = plant_own(&b, &b_tmp);
            facts.b_tmp = b_tmp;
            let (before, run_rows) = rows(&facts, variant, &own_lease);
            let args = argv(&act, &before, &run_rows);
            let control = control_b.clone();
            let out = tokio::task::spawn_blocking(move || control.run(&args))
                .await
                .expect("control run");
            drop(own);
            let said = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(
                out.status.success(),
                "B's control run: {out:?}\n{}",
                slot_report(&users.0)
            );
            let found = results(&said, &run_rows)
                .unwrap_or_else(|why| panic!("B's control run: {why}\n{said}{err}"));
            table.record("control", &run_rows, found);
        }
        table.print();
        assert!(!table.order.is_empty(), "no row was run");
        // The fixtures go before A's lease ends, so its clean meets none of them.
        planted.remove();
        std::fs::write(a_root.join("done"), b"").expect("release lease A");
    };

    let (a_result, ()) = tokio::join!(run_a, orchestrate);
    let result = a_result.expect("lease A ran");
    assert_eq!(
        result.exit_code,
        0,
        "lease A: {}{}",
        stdout(&cas, &result),
        stderr(&cas, &result)
    );
}
