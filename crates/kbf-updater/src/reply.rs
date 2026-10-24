//! The answer of an HTTP/1.1 call, read from the bytes a `Connection: close` exchange
//! returned. Shared by the two users of the operator API: safe-to-stop and node health.
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
    /// A body is as long as its `Content-Length` says, or is `Transfer-Encoding: chunked` (the
    /// frontend's proxy may send one) and then read whole and strictly; both together, and any
    /// other encoding, are refused. A body shorter than its `Content-Length` is a connection
    /// that was cut: no answer.
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
        let mut chunked = false;
        for h in response.headers.iter() {
            if h.name.eq_ignore_ascii_case("transfer-encoding") {
                if !h.value.eq_ignore_ascii_case(b"chunked") {
                    return Err(PortError::other(format!(
                        "{what} sent an encoding of the answer that it does not use"
                    )));
                }
                chunked = true;
            }
            if h.name.eq_ignore_ascii_case("content-length") {
                length = std::str::from_utf8(h.value)
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok());
            }
        }
        let mut body = &raw[head..];
        if chunked {
            if length.is_some() {
                return Err(PortError::other(format!(
                    "{what} sent a chunked answer with a length"
                )));
            }
            let body = dechunk(body).ok_or_else(|| {
                PortError::unreachable(format!("{what} sent a chunked answer that is cut or wrong"))
            })?;
            return Ok(Reply { status, body });
        }
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

/// The body of a chunked message: each chunk is its size in hex digits (no sign, no space; an
/// extension after `;` is ignored), CRLF, that many bytes, CRLF; the last has size 0 and may be
/// followed by trailers, which are not read. `None` for anything else, a message cut short
/// included.
fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..end]).ok()?;
        let digits = size.split(';').next().unwrap_or_default();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let size = usize::from_str_radix(digits, 16).ok()?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Some(body);
        }
        let chunk = rest.get(..size)?;
        if rest.get(size..size.checked_add(2)?)? != b"\r\n" {
            return None;
        }
        body.extend_from_slice(chunk);
        rest = &rest[size + 2..];
    }
}
