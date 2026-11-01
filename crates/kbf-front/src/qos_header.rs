//! The QoS an Execute asks for: the `x-kbf-qos` request header.
//!
//! A client says how urgent its work is with the header `x-kbf-qos: <level>` (a gRPC
//! metadata entry; Buck2 sends one with its `http_headers` setting). It is not a
//! platform property, so it is not part of the action digest and does not split the
//! action cache: the same action asked for at `ci` and at `batch` is the same key.
//!
//! What the front end accepts is a list the operator sets (`--qos-allowed`, `ci,batch`
//! unless changed). A call with no header, or a header naming a level that is not on
//! the list (`interactive` by default), or a value that is no level at all, runs at
//! `ci` and is never refused: a wrong header must not fail a build, only lose the
//! urgency it asked for. `ci` is the fallback, so it is accepted whatever the list says.

use std::fmt;
use std::str::FromStr;

use kbf_types::Qos;
use tonic::codegen::http;

/// The request header that names the level.
pub const QOS_HEADER: &str = "x-kbf-qos";

/// The longest header value echoed into a log line, in bytes.
const ECHOED: usize = 32;

/// The levels a call may ask for with [`QOS_HEADER`]. `ci` is always among them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QosAllow {
    interactive: bool,
    batch: bool,
}

/// A `--qos-allowed` value that is not a comma-separated list of `interactive`, `ci`
/// and `batch`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a list of QoS levels from interactive, ci and batch, separated by commas")]
pub struct BadQosAllow(String);

/// What a call asked for, and the level it gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    /// The level the action is submitted at.
    pub qos: Qos,
    /// The header value (cut to a log-sized prefix) that was asked for and not
    /// granted, if there was one.
    pub ignored: Option<String>,
}

impl QosAllow {
    /// `ci` and `batch`: the flag's default.
    pub const DEFAULT: Self = Self {
        interactive: false,
        batch: true,
    };

    fn allows(self, qos: &Qos) -> bool {
        match qos {
            Qos::Batch => self.batch,
            Qos::Interactive => self.interactive,
            // `ci` is the fallback, so always granted. A header never parses to a
            // custom level (they carry an urgency a name alone does not).
            other => *other == Qos::Ci,
        }
    }

    /// The level `headers` ask for, if the list grants it; `ci` otherwise.
    #[must_use]
    pub fn choose(self, headers: &http::HeaderMap) -> Chosen {
        let ci = |ignored| Chosen {
            qos: Qos::Ci,
            ignored,
        };
        let mut values = headers.get_all(QOS_HEADER).iter();
        let Some(value) = values.next() else {
            return ci(None);
        };
        let text = String::from_utf8_lossy(value.as_bytes());
        let asked = text.trim().to_ascii_lowercase();
        // Two headers are one too many to trust: take neither.
        let granted = values.next().is_none().then(|| asked.parse::<Qos>().ok());
        match granted.flatten() {
            Some(qos) if self.allows(&qos) => Chosen { qos, ignored: None },
            _ => ci(Some(asked.chars().take(ECHOED).collect())),
        }
    }
}

impl Default for QosAllow {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl FromStr for QosAllow {
    type Err = BadQosAllow;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut allow = Self {
            interactive: false,
            batch: false,
        };
        for name in s.split(',') {
            match name {
                "ci" => {}
                "batch" => allow.batch = true,
                "interactive" => allow.interactive = true,
                _ => return Err(BadQosAllow(s.to_owned())),
            }
        }
        Ok(allow)
    }
}

impl fmt::Display for QosAllow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ci")?;
        if self.batch {
            f.write_str(",batch")?;
        }
        if self.interactive {
            f.write_str(",interactive")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&[u8]]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for v in values {
            map.append(
                QOS_HEADER,
                http::HeaderValue::from_bytes(v).expect("a value"),
            );
        }
        map
    }

    fn level(allow: QosAllow, values: &[&[u8]]) -> Qos {
        allow.choose(&headers(values)).qos
    }

    /// Catches: a call without the header leaving `ci` (a missing header must never
    /// change what every existing client gets).
    #[test]
    fn no_header_is_ci() {
        let chosen = QosAllow::DEFAULT.choose(&http::HeaderMap::new());
        assert_eq!(
            chosen,
            Chosen {
                qos: Qos::Ci,
                ignored: None
            }
        );
    }

    /// Catches: `batch` not granted by default, `ci` not granted, and the case and the
    /// padding of the value mattering (a client may send `Batch` or ` batch`).
    #[test]
    fn ci_and_batch_are_granted_in_any_case() {
        let allow = QosAllow::DEFAULT;
        assert_eq!(level(allow, &[b"ci"]), Qos::Ci);
        assert_eq!(level(allow, &[b"batch"]), Qos::Batch);
        assert_eq!(level(allow, &[b"BATCH"]), Qos::Batch);
        assert_eq!(level(allow, &[b" Batch "]), Qos::Batch);
        assert_eq!(
            QosAllow::DEFAULT.choose(&headers(&[b"batch"])).ignored,
            None
        );
    }

    /// Catches: any client taking the most urgent level by asking for it (the default
    /// list must not carry `interactive`), and a refused ask being silent: it is
    /// reported so the caller logs it.
    #[test]
    fn interactive_is_ci_unless_the_list_has_it() {
        let asked = QosAllow::DEFAULT.choose(&headers(&[b"interactive"]));
        assert_eq!(
            asked,
            Chosen {
                qos: Qos::Ci,
                ignored: Some("interactive".to_owned())
            }
        );
        let open: QosAllow = "ci,batch,interactive".parse().unwrap();
        assert_eq!(level(open, &[b"interactive"]), Qos::Interactive);
    }

    /// Catches: a list without `batch` still granting it, and a list that names only
    /// `ci` refusing `ci`.
    #[test]
    fn a_list_without_batch_refuses_batch() {
        let only_ci: QosAllow = "ci".parse().unwrap();
        assert_eq!(level(only_ci, &[b"batch"]), Qos::Ci);
        assert_eq!(level(only_ci, &[b"ci"]), Qos::Ci);
    }

    /// Catches: a value that is no level failing the call or becoming a level: it is
    /// `ci`, and what is echoed for the log is cut short and lower-cased text, never
    /// raw bytes.
    #[test]
    fn garbage_is_ci_and_is_reported_short() {
        let allow = QosAllow::DEFAULT;
        for garbage in [&b"urgent"[..], b"", b"batch,ci", b"\xff\xfe", b"bat ch"] {
            let chosen = allow.choose(&headers(&[garbage]));
            assert_eq!(chosen.qos, Qos::Ci, "{garbage:?}");
            assert!(chosen.ignored.is_some(), "{garbage:?}");
        }
        let long = vec![b'x'; 500];
        let echoed = allow.choose(&headers(&[&long])).ignored.unwrap();
        assert_eq!(echoed.len(), ECHOED);
    }

    /// Catches: a request with two headers taking the first (or the last) one: a
    /// client or proxy that sends both is ambiguous, and the safe reading is `ci`.
    #[test]
    fn two_headers_are_ci() {
        assert_eq!(level(QosAllow::DEFAULT, &[b"batch", b"batch"]), Qos::Ci);
    }

    /// Catches: a flag value that does not print back to what parses, an empty list or
    /// an unknown name accepted, and `ci` missing from what a list prints.
    #[test]
    fn the_flag_value_round_trips_and_refuses_what_is_not_a_level() {
        for text in ["ci", "ci,batch", "ci,batch,interactive"] {
            assert_eq!(text.parse::<QosAllow>().unwrap().to_string(), text);
        }
        assert_eq!("batch".parse::<QosAllow>().unwrap().to_string(), "ci,batch");
        assert_eq!(QosAllow::DEFAULT.to_string(), "ci,batch");
        for bad in ["", "ci,", "urgent", "Batch", "ci batch"] {
            assert!(bad.parse::<QosAllow>().is_err(), "{bad:?}");
        }
    }
}
