//! [`kbf_transfer::RoutedCas`] over a one-member farm whose node is a fake that can lie,
//! stall and fail. (The conformance checks over real storage nodes are in
//! `kbf-cas-conformance`.)

mod support;

mod errors;
mod limits;
mod missing;
