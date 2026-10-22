//! What a storage node says of the build it runs and of its metadata log
//! (`RegisterRequest` fields 10 to 12, `HeartbeatRequest` fields 24 to 26): kept in its
//! member row (tags 7 to 9), shown by `GET /v1/members` and read by [`Members::node_facts`].
//! Information only: no decision of the frontend depends on it yet.
//!
//! **Absent is not zero.** A node that says nothing of a fact (an older build, or a node
//! with no log) leaves it `None`, and so does a row written before the fields existed.
//! Nothing turns "not said" into a number, so a reader that needs a fact must treat `None`
//! as unknown.
//!
//! **Who writes what.** A registration replaces all three facts, so a node restarted on
//! another build shows that build. A heartbeat replaces `log_version` (a log is raised while
//! the node runs) and keeps what the row holds of the other two, which cannot change without
//! a restart; a field the row does not hold takes the heartbeat's.
//!
//! **A row an older frontend rewrote.** A frontend built before these tags does not know
//! them and drops them when it rewrites a row (a heartbeat, a state change), as it drops the
//! end of a planned restart (`row.rs`). The facts of that row read `None` until the node's
//! next heartbeat to a frontend that knows them brings all three back. While an older
//! frontend serves the same store and receives the beats, they stay `None`.
//!
//! [`Members::node_facts`]: super::Members::node_facts

use kbf_proto::kbf::map::v1 as pb;

/// The longest `build_commit` a row keeps, in bytes: a full commit is 40 hex digits.
pub const MAX_BUILD_COMMIT: usize = 64;

/// What a node said of itself: see the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeFacts {
    /// The newest metadata-log machine version the node's build can raise its log to.
    pub runs_log: Option<u32>,
    /// The machine version its metadata log is at now.
    pub log_version: Option<u32>,
    /// The commit the node's binary was built from: at most [`MAX_BUILD_COMMIT`] bytes of
    /// what it said are kept.
    pub build_commit: Option<String>,
}

impl NodeFacts {
    /// What a registration says.
    #[must_use]
    pub fn from_register(request: &pb::RegisterRequest) -> Self {
        Self {
            runs_log: request.runs_log,
            log_version: request.log_version,
            build_commit: request.build_commit.clone(),
        }
    }

    /// What a heartbeat says.
    #[must_use]
    pub fn from_beat(beat: &pb::HeartbeatRequest) -> Self {
        Self {
            runs_log: beat.runs_log,
            log_version: beat.log_version,
            build_commit: beat.build_commit.clone(),
        }
    }

    /// What a row holding `self` holds after a heartbeat that said `beat`: the heartbeat's
    /// `log_version`, and of the other two what the row had, or the heartbeat's when the
    /// row had none.
    pub(super) fn after_beat(&self, beat: &Self) -> Self {
        Self {
            runs_log: self.runs_log.or(beat.runs_log),
            log_version: beat.log_version,
            build_commit: self
                .build_commit
                .clone()
                .or_else(|| beat.build_commit.clone()),
        }
    }
}

/// `text` cut to at most [`MAX_BUILD_COMMIT`] bytes, at a character boundary.
pub(super) fn capped(text: &str) -> String {
    let mut end = text.len().min(MAX_BUILD_COMMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
