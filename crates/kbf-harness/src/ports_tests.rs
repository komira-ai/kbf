//! The port claims: that blocks held together are disjoint, that a block is let go and that
//! a busy one is skipped, and where the ports lie.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::{Arc, Barrier};

use super::*;

fn floor() -> u16 {
    ephemeral_floor(
        &std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap_or_default(),
    )
}

#[test]
fn claims_held_at_once_never_share_a_block() {
    // Catches: two farms of one process (parallel tests) given one block, the failure of the
    // probe-and-close ports this replaces: a role dying with "address in use" or two roles
    // sharing a port. Eight threads claim from the same place at the same moment. Mutant: no
    // `flock` in `try_block`.
    let barrier = Arc::new(Barrier::new(8));
    let claims: Vec<Claim> = (0..8)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                Claim::starting_at(0)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().expect("a claim"))
        .collect();
    let indexes: BTreeSet<u32> = claims.iter().map(Claim::index).collect();
    assert_eq!(indexes.len(), 8, "{claims:?}");
    let bases: BTreeSet<u16> = claims.iter().map(Claim::base).collect();
    assert_eq!(bases.len(), 8);
    for pair in bases.iter().collect::<Vec<_>>().windows(2) {
        assert!(pair[0] + BLOCK <= *pair[1], "blocks overlap: {bases:?}");
    }
}

#[test]
fn two_farms_started_together_are_told_disjoint_ports_and_distinct_blocks() {
    // Catches: the farms of two tests of one process picking the same ports (the old
    // `ports()` probed the kernel for each, and `backend_base` walked from the process id).
    // Mutant: `Claim::new` always claims block 0
    // (the second claim then fails).
    let farm = || {
        std::thread::spawn(|| {
            let claim = Claim::new();
            let ports = Ports::of(&claim, 3);
            (claim, ports)
        })
    };
    let (a, b) = (farm(), farm());
    let ((ca, pa), (cb, pb)) = (a.join().expect("a farm"), b.join().expect("a farm"));
    let (all_a, all_b): (BTreeSet<u16>, BTreeSet<u16>) = (
        pa.all().into_iter().collect(),
        pb.all().into_iter().collect(),
    );
    assert!(all_a.is_disjoint(&all_b), "{pa:?} / {pb:?}");
    assert_ne!(ca.index(), cb.index());
    assert!(pa.front.0.abs_diff(pb.front.0) >= BLOCK);
}

#[test]
fn a_block_is_let_go_when_its_claim_is_dropped() {
    // Catches: a claim that is never released (the lock file leaked), which would use up
    // the blocks of a host in a few hundred runs of one long-lived process. Mutant: the
    // claim is `mem::forget`-ten.
    let claim = Claim::starting_at(190);
    let index = claim.index();
    assert!(Claim::try_block(index).is_none(), "held while it lives");
    drop(claim);
    assert!(Claim::try_block(index).is_some(), "free once dropped");
}

#[test]
fn a_block_with_a_busy_port_is_skipped() {
    // Catches: a block handed out although a program outside the harness listens on one of
    // its ports (on the second loopback address, a node's, which a probe of the first alone
    // would not see). Mutant: `try_block` does not probe.
    let claim = Claim::starting_at(100);
    let (index, base) = (claim.index(), claim.base());
    drop(claim);
    let busy = TcpListener::bind((std::net::Ipv4Addr::new(127, 0, 0, 2), base + 7))
        .expect("a node's address");
    let next = Claim::starting_at(index);
    assert_ne!(next.index(), index);
    drop(busy);
}

#[test]
fn every_port_lies_in_the_block_below_the_ephemeral_range() {
    // Catches: ports in the range the kernel gives to connections (what binding port 0
    // yields), where a connection's source port can take one before its role binds it.
    // Mutant: `FIRST` 20000 -> 40000.
    let claim = Claim::starting_at(120);
    let ports = Ports::of(&claim, MAX_NODES);
    let all = ports.all();
    assert_eq!(all.len(), 4 + 2 * MAX_NODES);
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), all.len());
    for port in all {
        assert!(port >= claim.base() && port < claim.base() + BLOCK);
        assert!(port >= FIRST && port < floor().min(EPHEMERAL), "{port}");
    }
    assert_eq!(
        ports.front.0,
        claim.base(),
        "the frontend's listeners open the block"
    );
    assert_eq!(ports.front.1, ports.front.0 + 1);
    assert_eq!(ports.front.2, ports.front.0 + 2);
}

#[test]
fn a_farm_of_fewer_nodes_has_fewer_ports() {
    // Catches: a node count that is not the port count.
    let claim = Claim::starting_at(130);
    let plain = Ports::of(&claim, 2);
    assert_eq!((plain.storage.len(), plain.metrics.len()), (2, 2));
    assert_eq!(plain.all().len(), 4 + 4);
}

#[test]
#[should_panic(expected = "a block holds ports for 8")]
fn more_nodes_than_a_block_holds_ports_for_is_refused() {
    // Catches: node 9's ports spilling into the next block's.
    let claim = Claim::starting_at(140);
    let _ = Ports::of(&claim, MAX_NODES + 1);
}

#[test]
#[should_panic(expected = "past the block")]
fn a_port_past_the_block_is_refused() {
    // Catches: an offset that walks into another farm's block.
    let claim = Claim::starting_at(141);
    let _ = claim.port(BLOCK);
}

#[test]
fn the_default_claim_is_a_fresh_one() {
    let claim = Claim::default();
    assert!(claim.base() >= FIRST);
}

#[test]
fn the_ephemeral_floor_is_read_from_the_kernels_text_or_defaults() {
    // Catches: blocks laid inside the ephemeral range on a host that lowers it, and a
    // parse that panics on text it does not know.
    assert_eq!(ephemeral_floor("32768\t60999\n"), 32768);
    assert_eq!(ephemeral_floor("1024 65535"), 1024);
    assert_eq!(ephemeral_floor(""), EPHEMERAL);
    assert_eq!(ephemeral_floor("many"), EPHEMERAL);
}

#[test]
fn the_number_of_blocks_follows_the_floor_and_never_passes_the_default() {
    assert_eq!(blocks(1024), 0);
    assert_eq!(blocks(FIRST + BLOCK - 1), 0);
    assert_eq!(blocks(FIRST + BLOCK), 1);
    assert_eq!(blocks(32768), 12768 / 64);
    assert_eq!(blocks(50000), 12768 / 64);
}

#[test]
fn the_search_wraps_round_the_blocks_once_and_gives_up() {
    // Catches: a search that never wraps (a farm started late in the range finds nothing),
    // one that starts past the end, and one that does not end.
    assert_eq!(search(5, 3, |i| (i == 1).then_some(i)), Some(1));
    assert_eq!(search(5, 13, |i| (i == 3).then_some(i)), Some(3));
    assert_eq!(search(5, 3, |_| None::<u32>), None);
    let tried = std::cell::RefCell::new(Vec::new());
    let _ = search(4, 2, |i| {
        tried.borrow_mut().push(i);
        None::<u32>
    });
    assert_eq!(*tried.borrow(), [2, 3, 0, 1]);
}

#[test]
#[should_panic(expected = "all 199 blocks of ports are held or busy")]
fn a_host_with_no_block_to_spare_is_told_so() {
    // Catches: a farm that starts with a block somebody else holds, or a failure that does
    // not say how many blocks there were. The search that finds nothing is the one that
    // ends in the panic.
    first_block(199, 7, |_| None::<u32>);
}

#[test]
fn the_first_block_found_is_the_one_claimed() {
    // Catches: the end of the search taken when a block was found.
    assert_eq!(first_block(5, 3, |i| (i == 1).then_some(i)), 1);
}

#[test]
fn the_lock_directory_and_files_are_open_to_every_user_whatever_the_umask() {
    // Catches: a second user of a shared temporary directory unable to open the first user's
    // lock files (made with the first user's umask, 0644), who then saw every block as held
    // and was told "all blocks are held or busy" on a host with none held. The directory is
    // open and sticky like the temporary directory, and the file of a claimed block is
    // read-write for all, under the umask this process has (022 here and in CI). Both are
    // first made narrow, as an older run (or a lock made by hand) left them, so the test
    // does not depend on what is in the temporary directory. The last block is claimed: the
    // other tests claim from the first. Mutants: no mode set on the file; none on the directory.
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |p: &std::path::Path| std::fs::metadata(p).expect("stat").permissions().mode();
    let last = blocks(floor()) - 1;
    make_lock_dir();
    let file = lock_dir().join(format!("block-{last}.lock"));
    let narrow = std::fs::Permissions::from_mode(0o600);
    let _ = std::fs::set_permissions(&file, narrow);
    std::fs::set_permissions(lock_dir(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    make_lock_dir();
    let claim = Claim::try_block(last).expect("the last block is free");
    assert_eq!(mode(&lock_dir()) & 0o1777, 0o1777);
    assert_eq!(mode(&file) & 0o666, 0o666);
    drop(claim);
}

/// Not a test: the process the one below runs again with another `TMPDIR`. Without
/// `KBF_PORTS_BLOCK` it does nothing; with it, it reports whether block `n` is held.
#[test]
fn block_probe_helper() {
    let (Ok(block), Ok(report)) = (
        std::env::var("KBF_PORTS_BLOCK"),
        std::env::var("KBF_PORTS_REPORT"),
    ) else {
        return;
    };
    let index: u32 = block.parse().expect("a block number");
    make_lock_dir();
    let word = if Claim::try_block(index).is_some() {
        "free"
    } else {
        "held"
    };
    std::fs::write(report, word).expect("report");
}

#[test]
fn a_harness_with_another_tmpdir_sees_the_lock() {
    // Catches: the lock directory following `TMPDIR`: two harnesses of one host with
    // different temporary directories lock different files and claim one block (the second
    // farm's roles then die with "address in use"). A second process, the test binary run
    // again with another `TMPDIR`, asks for the block this one holds. Mutant: `lock_dir`
    // is `std::env::temp_dir().join(..)` again (the other process finds the block free).
    let claim = Claim::starting_at(150);
    let dir = crate::scratch("ports-tmpdir");
    let (tmp, report) = (dir.join("other-tmp"), dir.join("report"));
    std::fs::create_dir_all(&tmp).expect("tmp");
    assert_ne!(std::env::temp_dir(), tmp);
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "ports::tests::block_probe_helper", "--nocapture"])
        .env("TMPDIR", &tmp)
        .env("KBF_PORTS_BLOCK", claim.index().to_string())
        .env("KBF_PORTS_REPORT", &report)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("the helper");
    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&report).expect("report"), "held");
    assert_eq!(lock_dir(), PathBuf::from(LOCK_DIR));
}
