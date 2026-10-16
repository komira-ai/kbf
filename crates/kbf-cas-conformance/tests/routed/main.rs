//! The routed data plane ([`kbf_transfer::RoutedCas`]): the conformance checks over real
//! storage nodes, and what the checks cannot reach over a one-member farm whose node is a
//! fake that can lie, stall and fail. One test binary, so that one copy of the code under
//! test is run by all of it.

mod conformance;
mod support;

mod errors;
mod limits;
mod missing;
