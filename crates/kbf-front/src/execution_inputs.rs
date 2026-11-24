//! What an Execute reads of its action before it is submitted: the Action, its Command and
//! its input tree, and on an action-cache hit the platform the cached result is for.
//!
//! The reads overlap where nothing orders them. The Action is read while the action cache is
//! looked up (`ExecutionService::execute_authorized`): a miss needs it next, and a hit
//! needs it to check the platform, so either answer uses the one read. Once the Action is
//! decoded, its Command is read while its input tree is walked (the walk's Directory reads and
//! its held-file check): the walk needs only the root digest the Action names. The answer is
//! made in the order the reads would have run one after another, so it does not depend on which
//! read ends first: a platform the Action carries is refused before the Command and the tree are
//! read at all; then a Command the store fails to read, that does not decode, or whose platform
//! is refused, is the answer over whatever the walk found (MISSING, or a store that failed);
//! then a walk that failed; and
//! only then the MISSING of every blob not held, the Command's first.
//!
//! The decoded Action and Command go to the dispatcher with the submission
//! ([`Submission::decoded`]), so the run's preparation reads neither from the CAS again.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::join;
use kbf_invocation::{BookSource, Evidence};
use kbf_proto::reapi::{Action, Command};
use kbf_storage::StorageError;
use kbf_types::{ActionKey, Digest};
use prost::Message;
use tokio::time::Instant;
use tonic::Status;

use super::{
    DecodedAction, ExecutionService, Presubmit, REFUSED_LOGGED, Submission, missing, platform,
    required, wants_whole_machine,
};
use crate::described::Described;
use crate::wire;

impl<D> ExecutionService<D> {
    /// The submission of `action`, whose bytes the Execute read (`None`: not held), once its
    /// inputs are all found held, with no call to record yet, and the Command digest the action
    /// names. The Command is read while the input tree is walked.
    pub(super) async fn submission(
        &self,
        instance: String,
        action: Digest,
        read: Option<Bytes>,
        window: &mut Presubmit<'_>,
    ) -> Result<(Submission, Digest), Status> {
        let Some(bytes) = read else {
            return Err(missing(&[action]));
        };
        let decoded = Action::decode(bytes).map_err(|e| {
            Status::invalid_argument(format!("action {action} does not decode: {e}"))
        })?;
        // A platform the Action carries is refused before anything else is read.
        let own = if platform(&decoded, None).is_some() {
            Some(wants_whole_machine(&decoded, None)?)
        } else {
            None
        };
        let command_digest = required(decoded.command_digest.as_ref(), "command_digest")?;
        let root = required(decoded.input_root_digest.as_ref(), "input_root_digest")?;

        // Before the Command is read and the walk begins: what the vouch below rests on.
        let reading = Instant::now();
        let walk = async {
            let walking = Instant::now();
            let walked = self.cache.walk_inputs(root).await;
            (walked, walking.elapsed())
        };
        let (command, (walked, walk_took)) = join(self.read_input(&command_digest), walk).await;
        window.walk = walk_took;
        self.cache
            .metrics()
            .execute_walk
            .observe_duration(window.walk);
        let mut absent = Vec::new();
        let command = match command? {
            Some(bytes) => Some(Command::decode(bytes).map_err(|e| {
                Status::invalid_argument(format!("command {command_digest} does not decode: {e}"))
            })?),
            None => {
                absent.push(command_digest);
                None
            }
        };
        // And a Command's platform, before anything the walk found is answered.
        let whole_machine = match own {
            Some(whole) => whole,
            None => wants_whole_machine(&decoded, command.as_ref())?,
        };
        let walked = walked?;
        let (input_bytes, input_files) = (walked.input_bytes, walked.input_files);
        window.dir_reads += walked.dirs_read;
        window.dirs_skipped += walked.dirs_skipped;
        window.dir_cache_hits += walked.dir_cache_hits;
        window.dirs_present_checked += walked.dirs_present_checked;
        window.dirs += walked.dirs_time;
        window.files += walked.files_time;
        window.file_asks += walked.file_asks;
        window.files_checked += walked.files_checked;
        self.observe_phases(&walked);
        absent.extend(walked.absent);
        if !absent.is_empty() {
            tracing::warn!(
                %action,
                count = absent.len(),
                first = %absent
                    .iter()
                    .take(REFUSED_LOGGED)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                "execute refused: input blob(s) are not in the CAS"
            );
            return Err(missing(&absent));
        }
        // The Command and every input held: the run's preparation need not ask again.
        if let Some(at) = walked.held_since {
            self.cache.vouch(action, at.min(reading));
        }
        // The Command is here: an absent one returned MISSING above.
        let described = command.clone().map(Described::of).unwrap_or_default();
        let decoded = command.map(|command| {
            Arc::new(DecodedAction {
                action: decoded,
                command,
            })
        });
        let submission = Submission {
            key: ActionKey { instance, action },
            whole_machine,
            call: None,
            described,
            evidence: Evidence {
                cpu_source: BookSource::default(),
                memory_source: BookSource::default(),
                input_bytes,
                input_files,
            },
            decoded,
        };
        Ok((submission, command_digest))
    }

    /// Refuses, for an Execute the action cache answers, the platform of the action that
    /// cached result is for, given the action's bytes as the Execute read them (`None`: not
    /// held): the Action's platform, else the Command's, as an Execute that ran would have
    /// read it. An action or Command the CAS does not hold, or that does not decode, is not
    /// refused here (an Execute that runs says so); a store that fails is the call's error.
    pub(super) async fn refuse_platform_of(&self, read: Option<Bytes>) -> Result<(), Status> {
        let Some(decoded) = read.and_then(|bytes| Action::decode(bytes).ok()) else {
            return Ok(());
        };
        let command = match decoded
            .command_digest
            .as_ref()
            .map(|d| wire::digest(Some(d)))
        {
            Some(Ok(digest)) if platform(&decoded, None).is_none() => {
                self.held::<Command>(&digest).await?
            }
            _ => None,
        };
        wants_whole_machine(&decoded, command.as_ref()).map(drop)
    }

    /// The message `digest` names, if the CAS holds it and it decodes.
    async fn held<T: prost::Message + Default>(
        &self,
        digest: &Digest,
    ) -> Result<Option<T>, Status> {
        Ok(self
            .read_input(digest)
            .await?
            .and_then(|bytes| T::decode(bytes).ok()))
    }

    /// The phases of a walk over a store in the metrics (a walk by the in-process cache
    /// has none to report: its figures are zero and it counts its own).
    fn observe_phases(&self, walked: &crate::input_tree::Walked) {
        let m = self.cache.metrics();
        m.execute_walk_dirs_read.add(walked.dirs_read);
        m.execute_walk_dirs_skipped.add(walked.dirs_skipped);
        m.execute_walk_file_asks.add(walked.file_asks);
        m.execute_walk_files_checked.add(walked.files_checked);
        for took in &walked.read_times {
            m.execute_walk_dir_read.observe_duration(*took);
        }
        m.execute_dir_cache_hits.add(walked.dir_cache_hits);
        m.execute_dir_cache_misses.add(walked.dir_cache_misses);
        m.execute_dir_cache_merged.add(walked.dir_cache_merged);
        m.execute_dir_cache_evictions.add(walked.dir_cache_evicted);
        m.execute_dir_cache_present_checked
            .add(walked.dirs_present_checked);
        if walked.phased {
            m.execute_dir_cache_bytes.set(walked.dir_cache_bytes);
        }
        if walked.phased {
            m.execute_walk_dirs.observe_duration(walked.dirs_time);
            m.execute_walk_files.observe_duration(walked.files_time);
        }
    }

    /// A blob's bytes, or `None` if it is not held. Anything else (unreachable, store
    /// down) fails the call: the input exists, so it is not MISSING.
    pub(super) async fn read_input(&self, digest: &Digest) -> Result<Option<Bytes>, Status> {
        match self.cache.read_blob(digest).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
