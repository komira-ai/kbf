//! A fake build client: for each of its actions it asks `FindMissingBlobs` for the
//! input, uploads it if missing and asks again, then `Execute`s the action, resending
//! the step it is on every [`RETRY`] until it is answered (as a client retries a
//! dropped stream).

use std::time::Duration;

use kbf_sim::{Event, NodeId, NodeInput, Output};
use kbf_types::{Digest, Effect, FarmTime, StateMachine};

use crate::{Msg, input_of};

/// How long a client waits for an answer before it sends its step again.
pub const RETRY: Duration = Duration::from_secs(8);

/// Where one action is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Upload,
    Check,
    Execute,
    Done,
}

/// One action a client runs, from `at` on.
#[derive(Clone, Debug)]
pub struct Job {
    pub action: Digest,
    /// When the client starts on it.
    pub at: FarmTime,
    pub step: Step,
    /// When it was answered.
    pub answered: Option<FarmTime>,
}

impl Job {
    fn message(&self) -> Option<Msg> {
        let input = input_of(&self.action);
        match self.step {
            Step::Upload => Some(Msg::Upload { digest: input }),
            Step::Check => Some(Msg::FindMissing {
                digests: vec![input],
            }),
            Step::Execute => Some(Msg::Execute {
                action: self.action,
            }),
            Step::Done => None,
        }
    }
}

/// The client node.
#[derive(Debug)]
pub struct FakeClient {
    server: NodeId,
    jobs: Vec<Job>,
    out: Vec<Output<Msg>>,
}

impl FakeClient {
    /// A client that runs `actions` against `server`.
    #[must_use]
    pub fn new(server: NodeId, actions: impl IntoIterator<Item = (Digest, FarmTime)>) -> Self {
        let jobs = actions
            .into_iter()
            .map(|(action, at)| Job {
                action,
                at,
                step: Step::Check,
                answered: None,
            })
            .collect();
        Self {
            server,
            jobs,
            out: Vec::new(),
        }
    }

    /// Its actions.
    #[must_use]
    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    fn send_all(&mut self, now: FarmTime) {
        let msgs: Vec<Msg> = self
            .jobs
            .iter()
            .filter(|j| j.at <= now)
            .filter_map(Job::message)
            .collect();
        for msg in msgs {
            self.send(msg);
        }
    }

    fn send(&mut self, msg: Msg) {
        let to = self.server.clone();
        self.out.push(Output::Send { to, msg });
    }

    /// Moves every job on `step` for which `matches` holds to the step `next` gives it,
    /// and sends that step.
    fn advance(&mut self, step: Step, matches: impl Fn(&Job) -> bool, next: impl Fn(&Job) -> Step) {
        let mut sends = Vec::new();
        for job in self.jobs.iter_mut().filter(|j| j.step == step) {
            if matches(job) {
                job.step = next(job);
                sends.extend(job.message());
            }
        }
        for msg in sends {
            self.send(msg);
        }
    }
}

impl StateMachine for FakeClient {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match input.event {
            Event::Start | Event::Timer { .. } => {
                self.send_all(now);
                self.out.push(Output::Timer {
                    after: RETRY,
                    tag: 0,
                });
            }
            Event::Message { msg, .. } => match msg {
                Msg::Uploaded { digest } => {
                    self.advance(
                        Step::Upload,
                        |j| input_of(&j.action) == digest,
                        |_| Step::Check,
                    );
                }
                Msg::Missing { asked, missing } => self.advance(
                    Step::Check,
                    |j| asked.contains(&input_of(&j.action)),
                    |j| {
                        if missing.contains(&input_of(&j.action)) {
                            Step::Upload
                        } else {
                            Step::Execute
                        }
                    },
                ),
                Msg::Answered { action, .. } => {
                    for job in self.jobs.iter_mut().filter(|j| j.action == action) {
                        if job.step != Step::Done {
                            job.step = Step::Done;
                            job.answered = Some(now);
                        }
                    }
                }
                _ => {}
            },
        }
        Vec::new()
    }
}
