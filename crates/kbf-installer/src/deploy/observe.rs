//! Which frontend the gates read, and how that follows a handover.
//!
//! On a host behind the frontend proxy the serving child changes with a handover: slot A
//! listens at `base_port`, slot B ten ports above, and the operator listener is at offset 2
//! of a slot (docs/design/frontend-proxy.md section 7; the offsets are the proxy's frozen
//! contract and are copied here, as this crate does not link the proxy). The proxy answers
//! `/metrics` itself and no `/v1` path carries the frontend's own series, so the gates
//! read the child's operator listener directly, and a deploy that hands the frontend over
//! must read the new child afterwards. [`Observe::resolve`] is called at the start and after
//! every frontend step.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use super::api::{Http, Operator, Request};

/// The slot stride, and the operator listener's offset in a slot.
pub const SLOT_STRIDE: u16 = 10;
pub const OPERATOR_OFFSET: u16 = 2;
/// The largest `base_port`: slot B's highest listener (offset 4) must still be a port.
pub const MAX_BASE_PORT: u16 = u16::MAX - SLOT_STRIDE - 4;

/// One of the proxy's two child slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub const ALL: [Slot; 2] = [Slot::A, Slot::B];

    /// The slot named by the update API's `active_slot` (`"a"` or `"b"`).
    pub fn parse(word: &str) -> Option<Slot> {
        match word {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Slot::A => "slot a",
            Slot::B => "slot b",
        })
    }
}

/// The operator listener of `slot` under `base_port`.
pub fn operator_port(base_port: u16, slot: Slot) -> u16 {
    match slot {
        Slot::A => base_port + OPERATOR_OFFSET,
        Slot::B => base_port + SLOT_STRIDE + OPERATOR_OFFSET,
    }
}

/// The slot that serves: the one the update API names; else the only one that answers
/// (`probe`). Two that answer with none named, or none, is an error: reading either would be
/// a guess.
pub fn choose(serving: Option<Slot>, probe: impl Fn(Slot) -> bool) -> Result<Slot, String> {
    if let Some(slot) = serving {
        return Ok(slot);
    }
    let live: Vec<Slot> = Slot::ALL.into_iter().filter(|&s| probe(s)).collect();
    match live.as_slice() {
        [one] => Ok(*one),
        [] => Err("no frontend child answers on slot a or slot b".to_owned()),
        _ => Err("both frontend slots answer and the update API names none as serving".to_owned()),
    }
}

/// A frontend to read, and what to call it in the result document.
pub struct Observed<'a> {
    pub label: String,
    pub api: Box<dyn Operator + 'a>,
}

/// Finds the frontend the gates read.
pub trait Observe {
    /// The frontend serving now. `serving` is the slot the update API reports, when it
    /// reports one.
    fn resolve(&self, serving: Option<Slot>) -> Result<Observed<'_>, String>;
}

/// One fixed address (`--observe`): a frontend that does not move.
pub struct Fixed {
    pub addr: SocketAddr,
    pub token: String,
    pub timeout: Duration,
}

impl Observe for Fixed {
    fn resolve(&self, _: Option<Slot>) -> Result<Observed<'_>, String> {
        let api =
            Http::new(self.addr, self.token.clone(), self.timeout).map_err(|e| e.to_string())?;
        Ok(Observed {
            label: format!("fixed {}", self.addr),
            api: Box::new(api),
        })
    }
}

/// The children of the frontend proxy (`--observe-base-port`): follows the serving slot.
pub struct Slots {
    pub base_port: u16,
    pub token: String,
    pub timeout: Duration,
}

impl Slots {
    fn client(&self, slot: Slot) -> Result<(SocketAddr, Http), String> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, operator_port(self.base_port, slot)));
        let http = Http::new(addr, self.token.clone(), self.timeout).map_err(|e| e.to_string())?;
        Ok((addr, http))
    }
}

impl Observe for Slots {
    fn resolve(&self, serving: Option<Slot>) -> Result<Observed<'_>, String> {
        let answers = |slot| {
            self.client(slot).is_ok_and(|(_, http)| {
                http.send(&Request::get("/metrics"))
                    .is_ok_and(|r| r.status == 200)
            })
        };
        let slot = choose(serving, answers)?;
        let (addr, http) = self.client(slot)?;
        Ok(Observed {
            label: format!("{slot} {addr}"),
            api: Box::new(http),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the slot's operator port follows the proxy's frozen offsets, and the serving
    /// slot is chosen: the API's word first, else the only child that answers; none or both
    /// answering is an error. Catches a swapped slot, a wrong offset, and a guess between
    /// two live children.
    ///
    /// Mutants planted: slot B's port computed as slot A's; the probe consulted when the API
    /// names a slot; two live children resolved to the first.
    #[test]
    fn the_serving_slot_is_named_or_the_only_one_alive() {
        assert_eq!(operator_port(18000, Slot::A), 18002);
        assert_eq!(operator_port(18000, Slot::B), 18012);
        assert_eq!(choose(Some(Slot::B), |_| true), Ok(Slot::B));
        assert_eq!(choose(None, |s| s == Slot::A), Ok(Slot::A));
        assert_eq!(choose(None, |s| s == Slot::B), Ok(Slot::B));
        assert!(
            choose(None, |_| false)
                .unwrap_err()
                .contains("no frontend child")
        );
        assert!(
            choose(None, |_| true)
                .unwrap_err()
                .contains("both frontend slots")
        );
        assert_eq!(Slot::parse("b"), Some(Slot::B));
        assert_eq!(Slot::parse("c"), None);
    }
}
