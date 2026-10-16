//! The map generation a standard call carries: the `kbf-map-generation` metadata entry
//! (the standard protocols' messages have no field for it).

use tonic::{Request, Status};

use super::MAP_GENERATION_HEADER;

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
