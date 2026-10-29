use kbf_lease_client::ErrorCode;

use super::{Settled, pinned, require};
use crate::executor::traits::{InputsError, PresignError};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Catches an image the lease server would refuse (or, worse, run from a mutable tag) being
/// posted: only `name@sha256:<64 hex>` is taken, and the platform property's `docker://` is
/// dropped.
#[test]
fn an_image_must_be_pinned_by_digest() {
    let image = format!("registry.test/app@sha256:{DIGEST}");
    assert_eq!(pinned(&image).as_deref(), Ok(image.as_str()));
    assert_eq!(pinned(&format!("docker://{image}")).as_deref(), Ok(image.as_str()));
    let tagged = format!("registry.test/app:1.0@sha256:{DIGEST}");
    assert_eq!(pinned(&tagged).as_deref(), Ok(tagged.as_str()));
    for bad in [
        "registry.test/app:latest".to_owned(),
        "registry.test/app@sha256:abc".to_owned(),
        format!("registry.test/app@sha256:{}z", &DIGEST[1..]),
        format!("registry.test/app@sha512:{DIGEST}"),
        String::new(),
    ] {
        let Err(Settled::Invalid(why)) = pinned(&bad) else {
            panic!("{bad:?} was taken");
        };
        assert!(why.contains("not pinned"), "{why}");
    }
}

/// Catches a user error (an invalid action) being classed as the farm's, and the farm's
/// (an unreachable store) as the user's: the first is never rerun.
#[test]
fn what_could_not_be_read_is_classed_by_whose_fault_it_is() {
    assert_eq!(
        Settled::from(InputsError::Invalid("no image".to_owned())),
        Settled::Invalid("no image".to_owned())
    );
    assert_eq!(
        Settled::from(InputsError::Unavailable("down".to_owned())),
        Settled::Infra("down".to_owned())
    );
    for error in [PresignError::Refused("ceiling".to_owned()), PresignError::Unavailable("ceiling".to_owned())] {
        let Settled::Infra(why) = Settled::from(error) else {
            panic!("not infra");
        };
        assert!(why.contains("ceiling"), "{why}");
    }
    assert_eq!(require(true, || unreachable!()), Ok(()));
    assert_eq!(
        require(false, || "no".to_owned()),
        Err(Settled::Invalid("no".to_owned()))
    );
    let _ = ErrorCode::NoCapacity;
}
