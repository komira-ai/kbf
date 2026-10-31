//! The operator API's enrollment DTOs (`docs/api.md`, `/v1/enroll/...`): what the frontend
//! sends and takes, in the one spelling the server and `kbf token` share.
//!
//! **Where a secret lives.** Only [`TokenCreated`], the answer to `POST /v1/enroll/tokens`,
//! holds the token's secret and its full string, and only that once: the registry keeps a
//! hash, [`TokenInfo`] has no field for the secret, its hash or the string, and the lists
//! are made of [`TokenInfo`] and [`NodeInfo`]. `TokenCreated`'s `Debug` prints neither.

use std::fmt;

use serde::{Deserialize, Serialize};

const fn one() -> u32 {
    1
}

/// `POST /v1/enroll/tokens`: what the operator asks of a new token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCreate {
    /// The roles it grants: `storage`, `daemon`, or `updater` alone.
    pub roles: Vec<String>,
    /// What the host runs (`frontend`, `storage`, `daemon` or `mac-daemon`): required with
    /// the role `updater`, which puts it in the host's certificate, and refused otherwise.
    #[serde(default)]
    pub host_role: Option<String>,
    /// How many nodes it can enroll: 1 to 1000. One when absent.
    #[serde(default = "one")]
    pub max_uses: u32,
    /// How long it lives, in seconds from now: at least 1, at most 30 days. A day when
    /// absent.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// The one node id it may enroll.
    #[serde(default)]
    pub node_id: Option<String>,
    /// The prefix every node id it enrolls must carry.
    #[serde(default)]
    pub node_prefix: Option<String>,
    /// Advertise hosts it allows: a host, or a suffix that starts with `.`. Empty allows any.
    #[serde(default)]
    pub names: Vec<String>,
    /// Whether it may enroll a node id that already exists.
    #[serde(default)]
    pub replace: bool,
    /// For the operator.
    #[serde(default)]
    pub label: String,
}

/// A token as the operator sees it: never its secret, its hash or its string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    /// Its lookup id.
    pub id: String,
    /// The roles it grants.
    pub roles: Vec<String>,
    /// What the host runs, for a token that grants `updater`.
    #[serde(default)]
    pub host_role: Option<String>,
    /// Uses allowed.
    pub max_uses: u32,
    /// Uses consumed.
    pub uses: u32,
    /// Store-clock seconds at which it expires.
    pub expires_at: u64,
    /// Store-clock seconds at which it was created.
    pub created_at: u64,
    /// Store-clock seconds at which it was revoked.
    pub revoked_at: Option<u64>,
    /// The exact node id it allows.
    pub node_id: Option<String>,
    /// The node id prefix it requires.
    pub node_prefix: Option<String>,
    /// The advertise names it allows.
    pub names: Vec<String>,
    /// Whether it may replace a node.
    pub replace: bool,
    /// For the operator.
    pub label: String,
    /// Who created it.
    pub created_by: String,
}

/// The answer to a create: the one time the secret and the full token string are shown.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreated {
    /// The full token string, `kbft1.<id>.<secret>.<pin>`.
    pub token: String,
    /// The secret part of it, base64url.
    pub secret: String,
    /// The token as a later list shows it.
    pub info: TokenInfo,
}

impl fmt::Debug for TokenCreated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenCreated")
            .field("id", &self.info.id)
            .finish_non_exhaustive()
    }
}

/// `GET /v1/enroll/tokens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenList {
    /// Every token, by id.
    pub tokens: Vec<TokenInfo>,
}

/// `DELETE /v1/enroll/tokens/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRevoked {
    /// The token revoked.
    pub id: String,
}

/// A node in the registry as the operator sees it. Certificates and keys are not in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// The node's id.
    pub id: String,
    /// The roles it holds.
    pub roles: Vec<String>,
    /// `active` or `revoked`.
    pub state: String,
    /// The `host:port` it advertises; empty for a node with no storage role.
    pub advertise: String,
    /// Its zone label.
    pub zone: String,
    /// The token it enrolled with.
    pub token_id: String,
    /// That token's label when it enrolled.
    pub token_label: String,
    /// Store-clock seconds of its enrollment.
    pub created_at: u64,
    /// Store-clock seconds of its last renewal.
    pub last_renewed_at: Option<u64>,
    /// When its newest certificate expires.
    pub not_after: u64,
    /// Store-clock seconds of its revocation.
    pub revoked_at: Option<u64>,
}

/// `GET /v1/enroll/nodes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeList {
    /// Every node, by id, revoked ones included.
    pub nodes: Vec<NodeInfo>,
}

/// `POST /v1/enroll/nodes/{id}/revoke`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRevoked {
    /// The node revoked.
    pub id: String,
    /// The `deny/` entries written, each as the deny list spells it (`node n1`).
    pub denied: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> TokenInfo {
        TokenInfo {
            id: "aaaaaaaaaaaa".to_owned(),
            roles: vec!["storage".to_owned()],
            host_role: Some("daemon".to_owned()),
            max_uses: 2,
            uses: 1,
            expires_at: 20,
            created_at: 10,
            revoked_at: Some(15),
            node_id: Some("n1".to_owned()),
            node_prefix: None,
            names: vec![".example".to_owned()],
            replace: true,
            label: "lab".to_owned(),
            created_by: "operator".to_owned(),
        }
    }

    /// Catches: the host role in a token's JSON spelled or placed differently: the exact
    /// bytes a client reads, `mac-daemon` with its hyphen, and an absent role as `null`.
    #[test]
    fn the_host_role_is_the_bare_name_in_the_json() {
        let mut one = info();
        one.host_role = Some("mac-daemon".to_owned());
        let json = serde_json::to_string(&one).expect("json");
        assert!(json.contains(r#""host_role":"mac-daemon","#), "{json}");
        one.host_role = None;
        let json = serde_json::to_string(&one).expect("json");
        assert!(json.contains(r#""host_role":null,"#), "{json}");
    }

    fn round<T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug>(value: &T) {
        let text = serde_json::to_string(value).expect("serialise");
        let back: T = serde_json::from_str(&text).expect("deserialise");
        assert_eq!(&back, value);
    }

    // Catches a DTO whose serde spelling drifts between the server (which writes it) and
    // `kbf token` (which reads it): every type must survive its own JSON.
    #[test]
    fn every_dto_round_trips_through_json() {
        round(&info());
        round(&TokenList {
            tokens: vec![info()],
        });
        round(&TokenRevoked { id: "a".to_owned() });
        round(&TokenCreated {
            token: "kbft1.x".to_owned(),
            secret: "s".to_owned(),
            info: info(),
        });
        let node = NodeInfo {
            id: "n1".to_owned(),
            roles: vec!["daemon".to_owned()],
            state: "active".to_owned(),
            advertise: String::new(),
            zone: "z".to_owned(),
            token_id: "t".to_owned(),
            token_label: "l".to_owned(),
            created_at: 1,
            last_renewed_at: None,
            not_after: 9,
            revoked_at: None,
        };
        round(&node);
        round(&NodeList { nodes: vec![node] });
        round(&NodeRevoked {
            id: "n1".to_owned(),
            denied: vec!["node/n1".to_owned()],
        });
    }

    // Catches a create body that needs fields the operator should not have to give (the
    // defaults are one use, a day, no policy), and one that quietly ignores a typo such as
    // `max_use`: an unknown field is refused.
    #[test]
    fn a_create_body_defaults_and_refuses_unknown_fields() {
        let want: TokenCreate = serde_json::from_str(r#"{"roles":["daemon"]}"#).expect("minimal");
        assert_eq!(
            want,
            TokenCreate {
                roles: vec!["daemon".to_owned()],
                host_role: None,
                max_uses: 1,
                expires_in: None,
                node_id: None,
                node_prefix: None,
                names: Vec::new(),
                replace: false,
                label: String::new(),
            }
        );
        round(&want);
        assert!(serde_json::from_str::<TokenCreate>(r#"{"roles":[],"max_use":3}"#).is_err());
    }

    // Catches a Debug that prints the secret or the token string (a log line or a failed
    // assertion would leak it): it names the id only.
    #[test]
    fn debug_of_a_created_token_shows_no_secret() {
        let created = TokenCreated {
            token: "kbft1.TOKENSTRING".to_owned(),
            secret: "SECRETPART".to_owned(),
            info: info(),
        };
        let shown = format!("{created:?}");
        assert!(!shown.contains("SECRETPART"), "{shown}");
        assert!(!shown.contains("TOKENSTRING"), "{shown}");
        assert!(shown.contains("aaaaaaaaaaaa"), "{shown}");
    }
}
