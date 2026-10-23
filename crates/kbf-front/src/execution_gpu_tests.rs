//! The GPU a lease books on a Mac, per lease kind.

use kbf_types::{LeaseKind, Platform};
use tonic::Code;

use super::{mac_gpus, needs};

fn request(os: Option<&str>) -> kbf_caps::Request {
    let mut platform = Platform::new();
    if let Some(os) = os {
        platform.insert("os", os).expect("os");
    }
    needs(&platform).expect("request")
}

/// Catches a lease kind other than `action` booking the host's GPU slot on a Mac: an
/// `action` books the one slot, a `whole_machine` lease books none on top, a `vm` lease
/// is refused, and on a request naming no OS every kind books what it asks.
#[test]
fn only_an_action_lease_books_the_macs_gpu_slot() {
    let mac = request(Some("macos"));
    assert_eq!(mac_gpus(LeaseKind::Action, 1, &mac).unwrap(), 1);
    assert_eq!(mac_gpus(LeaseKind::WholeMachine, 1, &mac).unwrap(), 0);
    let refused = mac_gpus(LeaseKind::Vm, 1, &mac).unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    for kind in LeaseKind::ALL {
        assert_eq!(mac_gpus(kind, 0, &mac).unwrap(), 0, "{kind:?}");
        assert_eq!(mac_gpus(kind, 1, &request(None)).unwrap(), 1, "{kind:?}");
        assert_eq!(
            mac_gpus(kind, 2, &mac).unwrap_err().code(),
            Code::InvalidArgument,
            "{kind:?}"
        );
    }
}
