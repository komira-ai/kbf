//! The map generation a standard call carries: the `kbf-map-generation` metadata entry
//! (the standard protocols' messages have no field for it).

use kbf_map::MemberId;
use tonic::{Request, Status};

use super::MAP_GENERATION_HEADER;
use crate::replicate::LEG_HEADER;

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
