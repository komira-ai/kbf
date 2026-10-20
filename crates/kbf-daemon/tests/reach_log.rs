//! What the storage network check tells the operator. Alone in its test binary: a tracing
//! callsite that another test's thread reached first, with no subscriber installed, is
//! cached as uninteresting, and a subscriber installed after would miss it.

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::reach::{Plan, check_first_map};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::MapHolder;

fn map_of(addrs: &[&str]) -> Map {
    let members = addrs
        .iter()
        .zip(1u64..)
        .map(|(addr, id)| Member {
            id: MemberId(id),
            addr: (*addr).to_owned(),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
            boot: 1,
        })
        .collect();
    Map::new(1, members, Rule::Rendezvous, 1, 0, Vec::new()).expect("a map")
}

/// Catches: an unreachable member that is dropped without a word (the operator learns of
/// it from the first failed call), a warning that does not name its address, and a
/// healthy start that warns.
#[tokio::test]
async fn an_unreachable_member_is_named_in_a_warning_and_a_healthy_start_is_quiet() {
    let log = Log::default();
    let _installed = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let up = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let up_addr = up.local_addr().expect("addr").to_string();
    // A low port nothing listens on.
    let gone_addr = "127.0.0.1:1".to_owned();
    let plan = Plan {
        within: Duration::from_secs(30),
        attempts: 1,
        pause: Duration::ZERO,
    };

    let healthy = Arc::new(MapHolder::new());
    assert!(healthy.offer(map_of(&[&up_addr])));
    check_first_map(healthy, None, plan).await.expect("passes");
    assert_eq!(log.count("WARN"), 0);
    assert_eq!(log.count("every storage member is reachable"), 1);

    let degraded = Arc::new(MapHolder::new());
    assert!(degraded.offer(map_of(&[&up_addr, &gone_addr])));
    check_first_map(degraded, None, plan)
        .await
        .expect("goes on");
    assert_eq!(log.count("WARN"), 1);
    assert_eq!(log.count(&gone_addr), 1);
    assert_eq!(
        log.count(&up_addr),
        0,
        "the member that answered is not named"
    );

    // Nobody answers: each failed round but the last is logged, then the refusal.
    let nobody = Arc::new(MapHolder::new());
    assert!(nobody.offer(map_of(&[&gone_addr])));
    let thrice = Plan {
        attempts: 3,
        ..plan
    };
    check_first_map(nobody, None, thrice)
        .await
        .expect_err("refused");
    assert_eq!(log.count("probing again"), 2);
}

/// What a subscriber wrote, for the test above.
#[derive(Clone, Default)]
struct Log(Arc<std::sync::Mutex<Vec<u8>>>);

impl Log {
    fn count(&self, needle: &str) -> usize {
        let text = String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8");
        text.matches(needle).count()
    }
}

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
