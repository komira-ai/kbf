//! The answer of an HTTP/1.1 call, read from the bytes a `Connection: close` exchange
//! returned. Shared by the three clients of this crate: the operator API (safe-to-stop and
//! node health) and kbf-proxy's swap status.
//!
//! Error texts carry no address and no token: they reach `GET /rolls/{id}`.

use crate::ports::PortError;

/// A status and a body.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// Read the answer in `raw`. `what` names the far side in the texts ("the operator API").
    /// A chunked answer is refused (none of these servers sends one), and a body shorter
    /// than its `Content-Length` is a connection that was cut: no answer.
    pub fn parse(raw: &[u8], what: &str) -> Result<Reply, PortError> {
        let mut slots = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut slots);
        let Ok(httparse::Status::Complete(head)) = response.parse(raw) else {
            return Err(PortError::other(format!(
                "{what} did not answer with HTTP/1.1"
            )));
        };
        let status = response.code.unwrap_or_default();
        let mut length = None;
        for h in response.headers.iter() {
            if h.name.eq_ignore_ascii_case("transfer-encoding") {
                return Err(PortError::other(format!(
                    "{what} sent a chunked answer, which it does not do"
                )));
            }
            if h.name.eq_ignore_ascii_case("content-length") {
                length = std::str::from_utf8(h.value)
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok());
            }
        }
        let mut body = &raw[head..];
        if let Some(length) = length {
            if body.len() < length {
                return Err(PortError::unreachable(format!(
                    "{what} closed the answer short"
                )));
            }
            body = &body[..length];
        }
        Ok(Reply {
            status,
            body: body.to_vec(),
        })
    }

    /// The error for a status the caller has no meaning for: the gates' refusals (401, 403,
    /// 415) are an answer that says no, `check` naming what an operator looks at; a 5xx is no
    /// answer; anything else is a surprise.
    pub fn failure(&self, what: &str, check: &str) -> PortError {
        match self.status {
            401 | 403 | 415 => PortError::other(format!(
                "{what} refused the call ({}); check {check}",
                self.status
            )),
            500..=599 => PortError::unreachable(format!("{what} answered {}", self.status)),
            _ => PortError::other(format!("{what} answered {}", self.status)),
        }
    }
}
