//! The map generation a standard call carries: the `kbf-map-generation` metadata entry
//! (the standard protocols' messages have no field for it).

use kbf_map::MemberId;
use tonic::{Request, Status};

use super::{DURABLE_HEADER, DURABLE_ONLY, DURABLE_WAIT, MAP_GENERATION_HEADER, UNOWNED_HEADER};
use crate::replicate::{LEG_HEADER, LEG_HELD_HEADER, LEG_HELD_VALUE};

/// The generation `request` was routed by: the decimal in its `kbf-map-generation`
/// entry, or 0 ("no map seen") when it has none.
///
/// # Errors
/// INVALID_ARGUMENT if the entry is not a decimal `u64`: a caller that cannot say which
/// map it used is not told it is stale or current.
pub(super) fn requested<T>(request: &Request<T>) -> Result<u64, Status> {
    let Some(value) = request.metadata().get(MAP_GENERATION_HEADER) else {
        return Ok(0);
    };
    value
        .to_str()
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| {
            Status::invalid_argument(format!("{MAP_GENERATION_HEADER} is not a decimal number"))
        })
}

/// Whether `request` is a write of a copy the map does not place on the node
/// ([`UNOWNED_HEADER`]).
pub(super) fn unowned<T>(request: &Request<T>) -> bool {
    request.metadata().get(UNOWNED_HEADER).is_some()
}

/// Whether a write is taken as an unowned copy: it asked to be one (`marked`), its caller
/// may ask ([`super::Admission::permits_unowned`]), and it is not a chain copy (`leg_from`):
/// a leg is a copy from another node, judged by the chain's rules for one, and the marker
/// on it changes nothing.
pub(super) fn takes_unowned(leg_from: Option<MemberId>, marked: bool, permitted: bool) -> bool {
    leg_from.is_none() && marked && permitted
}

/// The member that sent `request` as a copy of a blob it holds (the `kbf-chain-leg`
/// entry), if the call is one.
///
/// # Errors
/// INVALID_ARGUMENT if the entry is not a decimal member id.
pub(super) fn leg<T>(request: &Request<T>) -> Result<Option<MemberId>, Status> {
    let Some(value) = request.metadata().get(LEG_HEADER) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|text| text.parse().ok())
        .map(|id| Some(MemberId(id)))
        .ok_or_else(|| Status::invalid_argument(format!("{LEG_HEADER} is not a member id")))
}

/// Whether `request`, a copy sent by `leg_from`, asks for a held answer
/// ([`LEG_HELD_HEADER`]). A call that is not a leg is a client's write, which has no say in
/// how a node acknowledges it: the entry is ignored there.
///
/// # Errors
/// INVALID_ARGUMENT if a leg sends the entry with any value but [`LEG_HELD_VALUE`]: a
/// sender that asked for something this node does not know is not answered as if it had
/// not asked.
pub(super) fn asks_held<T>(
    request: &Request<T>,
    leg_from: Option<MemberId>,
) -> Result<bool, Status> {
    let Some(value) = request.metadata().get(LEG_HELD_HEADER) else {
        return Ok(false);
    };
    if leg_from.is_none() {
        return Ok(false);
    }
    match value.to_str() {
        Ok(LEG_HELD_VALUE) => Ok(true),
        _ => Err(Status::invalid_argument(format!(
            "{LEG_HELD_HEADER} must be {LEG_HELD_VALUE}"
        ))),
    }
}

/// Which answer a `FindMissingBlobs` asks for ([`DURABLE_HEADER`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Durability {
    /// No entry: a held blob is present.
    Servable,
    /// `only`: the index alone.
    Only,
    /// `wait`: the index alone, after the held blobs have been given time to be written.
    Wait,
}

/// The answer `request` asks for ([`DURABLE_HEADER`]).
///
/// # Errors
/// INVALID_ARGUMENT if the entry has any value but `only` or `wait`: a caller that asked a
/// question this node does not know is not answered the plain one.
pub(super) fn durability<T>(request: &Request<T>) -> Result<Durability, Status> {
    let Some(value) = request.metadata().get(DURABLE_HEADER) else {
        return Ok(Durability::Servable);
    };
    match value.to_str() {
        Ok(DURABLE_ONLY) => Ok(Durability::Only),
        Ok(DURABLE_WAIT) => Ok(Durability::Wait),
        _ => Err(Status::invalid_argument(format!(
            "{DURABLE_HEADER} must be {DURABLE_ONLY} or {DURABLE_WAIT}"
        ))),
    }
}
