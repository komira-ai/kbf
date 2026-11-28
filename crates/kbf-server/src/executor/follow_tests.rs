//! The follower: one listing a poll for every job of the process past its first steps, and
//! what each followed job does with what a listing tells it. The rig's clock is paused, so
//! every call is counted at the step of the clock it went out at.

use std::collections::BTreeMap;
use std::time::Duration;

use js_proto::v1::{Job, JobSpec, JobState, ListJobsResponse};
use kbf_front::{Dispatch as _, Finished};
use kbf_job_client::fake::{Call, Fault};
use kbf_job_client::{Client, Error, Stuck};
use prost::Message as _;
use tonic::Code;

use super::{LIVE, News, PAGE, Registry, live_jobs, names, news};
use crate::executor::submit::INSTANCE_LABEL;
use crate::executor::tests::Rig;

/// One step of the rig's clock.
const TICK: Duration = Duration::from_millis(10);

/// How many jobs the rig's executor follows by listing.
fn following(rig: &Rig) -> usize {
    rig.exec.inner().follower.following()
}

/// Every call the job service was sent.
fn requests(rig: &Rig) -> usize {
    rig.fake.calls().len()
}

/// The reads of jobs the job service was sent: its `GetJob` and its `ListJobs`.
fn reads(rig: &Rig) -> usize {
    rig.calls_of("GetJob") + rig.calls_of("ListJobs")
}

/// A job read back as `id` in `state`, labelled by `instance` when there is one.
fn job(id: &str, state: JobState, instance: Option<&str>) -> Job {
    let labels: BTreeMap<String, String> = instance
        .map(|instance| (INSTANCE_LABEL.to_owned(), instance.to_owned()))
        .into_iter()
        .collect();
    Job {
        id: id.to_owned(),
        state: state.into(),
        spec: Some(JobSpec {
            labels,
            ..JobSpec::default()
        }),
        ..Job::default()
    }
}

/// The answer of one `ListJobs` page holding `jobs`, with `next` as its next page token.
fn page(jobs: Vec<Job>, next: &str) -> Fault {
    let page = ListJobsResponse {
        jobs,
        next_page_token: next.to_owned(),
    };
    Fault::Answer(bytes::Bytes::from(page.encode_to_vec()))
}

/// The page tokens of the listings the job service was sent, in order.
fn tokens(rig: &Rig) -> Vec<String> {
    rig.fake
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::ListJobs(request) => Some(request.page_token),
            _ => None,
        })
        .collect()
}

/// The news `heard` holds now.
fn heard(heard: &tokio::sync::watch::Receiver<Option<News>>) -> Option<News> {
    heard.borrow().clone()
}

/// Steps the clock until a listing goes out, and says how many steps that took.
async fn next_listing(rig: &Rig) -> u64 {
    let listed = rig.calls_of("ListJobs");
    for tick in 1..=100 {
        rig.advance(TICK).await;
        if rig.calls_of("ListJobs") > listed {
            return tick;
        }
    }
    panic!("no listing in a second");
}

/// Catches the executor reading every running job on its own each poll, four requests a job a
/// second at the default poll: with twenty jobs running for ten seconds, the job service is sent
/// one listing a poll for all of them, 40 requests where reads of each would be 800, which is
/// 0.2 requests a running job a second.
#[tokio::test(start_paused = true)]
async fn running_jobs_cost_the_service_under_one_request_a_job_a_second() {
    const JOBS: usize = 20;
    const SECONDS: usize = 10;
    let rig = Rig::start().await;
    let mut callers = Vec::new();
    for tag in 1..=20 {
        let caller = rig.submit(tag);
        let job = rig.submitted(&caller, 0).await;
        rig.host_runs(&job);
        callers.push((caller, job));
    }
    // Two seconds: past every job's steps, which end 310 ms after its first read.
    for _ in 0..8 {
        rig.advance(rig.cfg.poll).await;
    }
    let before = requests(&rig);
    let polls = SECONDS * 4;
    for _ in 0..polls {
        rig.advance(rig.cfg.poll).await;
    }
    let sent = requests(&rig) - before;
    let per_job_second = sent as f64 / (JOBS * SECONDS) as f64;
    assert!(
        per_job_second < 1.0,
        "{sent} requests for {JOBS} running jobs over {SECONDS} s: {per_job_second} a job a second"
    );
    assert_eq!(
        sent, polls,
        "one listing a poll for every job, and nothing else"
    );
    for (caller, job) in &callers {
        assert!(rig.fake.finish(job, 0));
        assert!(matches!(rig.until_done(caller).await, Finished::Ran(_)));
    }
}

/// Catches a followed job's end seen later than its own read a poll after the last would see
/// it: a job that ends just after a listing is seen at the next listing, a poll later, read once
/// on its own and its result read in the same step, and its callers are answered.
#[tokio::test(start_paused = true)]
async fn a_followed_job_that_ends_is_seen_at_the_next_listing() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    rig.until("the job is followed", || following(&rig) == 1)
        .await;
    next_listing(&rig).await;
    assert!(rig.fake.finish(&job, 0));
    let (reads, results) = (rig.calls_of("GetJob"), rig.calls_of("GetResult"));
    let steps = next_listing(&rig).await;
    assert_eq!(
        steps * 10,
        250,
        "seen at the next listing, a poll after the last"
    );
    assert_eq!(rig.calls_of("GetJob"), reads + 1, "read once on its own");
    assert_eq!(
        rig.calls_of("GetResult"),
        results + 1,
        "its result in the same step"
    );
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(following(&rig), 0);
}

/// Catches a cancel not heard while a job waits for the listing, or a listing that goes on with
/// no job to follow: the cancel withdraws the job and answers `CANCELLED`, the job is no longer
/// followed, and no listing goes out after it.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_followed_withdraws_the_job_and_the_listings_stop() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    rig.until("the job is followed", || following(&rig) == 1)
        .await;
    let cancels = rig.calls_of("CancelJob");
    assert!(rig.exec.cancel(&caller.name));
    match rig.until_done(&caller).await {
        Finished::Failed(status) => assert_eq!(status.code, Code::Cancelled as i32),
        other => panic!("not cancelled: {other:?}"),
    }
    assert_eq!(
        rig.calls_of("CancelJob"),
        cancels + 1,
        "the job is withdrawn"
    );
    assert_eq!(following(&rig), 0);
    let listed = rig.calls_of("ListJobs");
    for _ in 0..4 {
        rig.advance(rig.cfg.poll).await;
    }
    assert_eq!(rig.calls_of("ListJobs"), listed, "no listing with no job");
}

/// Catches a busy or unreachable job service ending a followed job's run: listings answered
/// `RESOURCE_EXHAUSTED` and `UNAVAILABLE`, and one whose connection is lost, are sent again,
/// the job that ended meanwhile answers its callers with its result, and no attempt is spent.
#[tokio::test(start_paused = true)]
async fn listings_on_a_busy_service_end_no_run() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    rig.until("the job is followed", || following(&rig) == 1)
        .await;
    let listed = rig.calls_of("ListJobs");
    for code in [
        Code::ResourceExhausted,
        Code::Unavailable,
        Code::Unavailable,
    ] {
        let message = "busy".to_owned();
        rig.fake
            .inject_on("ListJobs", Fault::Status { code, message });
    }
    rig.fake.inject_on("ListJobs", Fault::LoseAnswer);
    rig.until("the faults are spent", || {
        rig.calls_of("ListJobs") >= listed + 4
    })
    .await;
    assert!(caller.finished().is_none(), "the run goes on");
    assert!(rig.fake.finish(&job, 0));
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 0);
}

/// Catches a listing refused for a reason other than the service's load leaving its jobs
/// unread, or sent again each poll on top of their reads: each followed job is read on its own
/// at once, is no longer followed, and is read on its own a poll later, with no listing.
#[tokio::test(start_paused = true)]
async fn a_listing_refused_otherwise_sends_each_job_back_to_its_own_reads() {
    let rig = Rig::start().await;
    let (one, two) = (rig.submit(1), rig.submit(2));
    let first = rig.submitted(&one, 0).await;
    let second = rig.submitted(&two, 0).await;
    rig.host_runs(&first);
    rig.host_runs(&second);
    rig.until("both jobs are followed", || following(&rig) == 2)
        .await;
    rig.fake.inject_on(
        "ListJobs",
        Fault::Status {
            code: Code::InvalidArgument,
            message: "no".to_owned(),
        },
    );
    let reads = rig.calls_of("GetJob");
    next_listing(&rig).await;
    assert_eq!(
        rig.calls_of("GetJob"),
        reads + 2,
        "each job read on its own at once"
    );
    assert_eq!(following(&rig), 0, "and no longer followed");
    let (reads, listed) = (rig.calls_of("GetJob"), rig.calls_of("ListJobs"));
    rig.advance(rig.cfg.poll).await;
    assert_eq!(
        rig.calls_of("GetJob"),
        reads + 2,
        "read on its own a poll later"
    );
    assert_eq!(
        rig.calls_of("ListJobs"),
        listed,
        "no listing with no job followed"
    );
    assert!(rig.fake.finish(&first, 0));
    assert!(rig.fake.finish(&second, 0));
    assert!(matches!(rig.until_done(&one).await, Finished::Ran(_)));
    assert!(matches!(rig.until_done(&two).await, Finished::Ran(_)));
}

/// Catches listings sent with nothing to follow: an executor with no job sends none, and one
/// whose last job ended sends no more.
#[tokio::test(start_paused = true)]
async fn an_executor_with_no_job_to_follow_lists_nothing() {
    let rig = Rig::start().await;
    for _ in 0..8 {
        rig.advance(rig.cfg.poll).await;
    }
    assert_eq!(reads(&rig), 0);
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    rig.until("the job is followed", || following(&rig) == 1)
        .await;
    assert!(rig.fake.finish(&job, 0));
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    let sent = reads(&rig);
    for _ in 0..8 {
        rig.advance(rig.cfg.poll).await;
    }
    assert_eq!(reads(&rig), sent, "no read after the last job");
}

/// Catches a job handed over while a listing is out taken to have ended because that listing
/// does not have it: only a listing that started after a job was handed over says it is
/// missing.
#[test]
fn a_job_handed_over_while_a_listing_is_out_is_not_missing_from_it() {
    let mut registry = Registry::default();
    let (early, start) = registry.join("early");
    assert!(start, "the first job starts the listing");
    let mark = registry.round().expect("a job is followed");
    let (late, start) = registry.join("late");
    assert!(!start, "the listing already runs");
    registry.tell(mark, Some(Vec::new()));
    assert_eq!(heard(&early), Some(News::Missing));
    assert_eq!(heard(&late), None, "the next listing says");
    let mark = registry.round().expect("jobs are followed");
    registry.tell(mark, Some(Vec::new()));
    assert_eq!(heard(&late), Some(News::Missing));
}

/// Catches a listed job told it is missing, or a job of another process taken as one of the
/// followed: each followed job is told what the listing has of it, and a listed job no one
/// follows is passed over.
#[test]
fn a_listing_tells_each_followed_job_what_it_has() {
    let mut registry = Registry::default();
    let (listed, _) = registry.join("listed");
    let (gone, _) = registry.join("gone");
    let mark = registry.round().expect("jobs are followed");
    let running = job("listed", JobState::Running, Some("me"));
    let other = job("other", JobState::Running, Some("me"));
    registry.tell(mark, Some(vec![running.clone(), other]));
    assert_eq!(heard(&listed), Some(News::Listed(Box::new(running))));
    assert_eq!(heard(&gone), Some(News::Missing));
    assert_eq!(registry.jobs.len(), 2, "no job is added by a listing");
}

/// Catches a failed listing that leaves its jobs waiting for ever: each followed job is told to
/// read itself, those handed over while it was out included.
#[test]
fn a_failed_listing_tells_every_job_to_read_itself() {
    let mut registry = Registry::default();
    let (one, _) = registry.join("one");
    let mark = registry.round().expect("a job is followed");
    let (two, _) = registry.join("two");
    registry.tell(mark, None);
    assert_eq!(heard(&one), Some(News::Unlisted));
    assert_eq!(heard(&two), Some(News::Unlisted));
}

/// Catches a listing task that outlives its last job, or a job handed over after it ended with
/// no task to list it: the round with no job ends the task, and the next job starts another.
#[test]
fn the_listing_ends_with_the_last_job_and_the_next_starts_it() {
    let mut registry = Registry::default();
    assert_eq!(registry.round(), None, "no job: no listing");
    let (_one, start) = registry.join("one");
    assert!(start);
    assert!(registry.round().is_some());
    registry.leave("one");
    assert_eq!(registry.round(), None, "the last job left");
    let (_two, start) = registry.join("two");
    assert!(start, "a new task lists the next job");
}

/// Catches a job of another process, or one whose read names no process, handed to this
/// process's listing, which never has it: only the label of this process hands a job over.
#[test]
fn only_a_job_labelled_with_this_process_is_followed_by_its_listing() {
    assert!(names(&job("a", JobState::Running, Some("me")), "me"));
    assert!(!names(&job("a", JobState::Running, Some("you")), "me"));
    assert!(!names(&job("a", JobState::Running, None), "me"));
    assert!(!names(&Job::default(), "me"));
}

/// Catches a listing that asks for ended jobs, another process's jobs, or the service's own page
/// size: it asks for the four live states, this process's label, and pages of [`PAGE`] jobs.
#[test]
fn the_listing_asks_for_the_live_jobs_of_this_process() {
    let request = live_jobs("me");
    let states: Vec<i32> = LIVE.into_iter().map(i32::from).collect();
    assert_eq!(request.states, states);
    assert_eq!(
        LIVE,
        [
            JobState::Queued,
            JobState::Placed,
            JobState::Starting,
            JobState::Running
        ]
    );
    let labels: Vec<(&str, &str)> = request
        .labels
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(labels, [(INSTANCE_LABEL, "me")]);
    assert_eq!(request.page_size, PAGE);
    assert!(request.page_token.is_empty());
    assert_eq!(request.client, None);
}

/// Catches a job whose follower can send no more news waiting for ever: with the sender gone it
/// hears none at once, and so reads on its own; with news sent it hears the latest.
#[tokio::test]
async fn a_job_hears_the_latest_news_or_none_once_no_more_can_come() {
    let (sender, mut heard) = tokio::sync::watch::channel(None);
    sender.send_replace(Some(News::Missing));
    sender.send_replace(Some(News::Unlisted));
    assert_eq!(news(&mut heard).await, Some(News::Unlisted));
    drop(sender);
    assert_eq!(news(&mut heard).await, None);
}

/// Catches a listing that stops at its first page, or asks for the next one from the start: the
/// jobs of every page are listed, and each page is asked for from the token the last one gave.
#[tokio::test(start_paused = true)]
async fn a_listing_follows_its_pages_from_token_to_token() {
    let rig = Rig::start().await;
    let first = job("a", JobState::Running, Some("me"));
    let second = job("b", JobState::Queued, Some("me"));
    rig.fake
        .inject_on("ListJobs", page(vec![first.clone()], "t1"));
    rig.fake
        .inject_on("ListJobs", page(vec![second.clone()], ""));
    let listed = rig.exec.inner().api().list_live("me").await;
    assert_eq!(listed.expect("listed"), [first, second]);
    assert_eq!(tokens(&rig), ["", "t1"]);
}

/// Catches a listing that asks for ever when the service gives a page token a second time: it
/// stops with that token.
#[tokio::test(start_paused = true)]
async fn a_listing_stops_at_a_page_token_given_twice() {
    let rig = Rig::start().await;
    rig.fake.inject_on("ListJobs", page(Vec::new(), "t1"));
    rig.fake.inject_on("ListJobs", page(Vec::new(), "t1"));
    let listed = rig.exec.inner().api().list_live("me").await;
    assert!(
        matches!(&listed, Err(Error::Stuck(Stuck::RepeatedToken(token))) if token == "t1"),
        "{listed:?}"
    );
    assert_eq!(tokens(&rig), ["", "t1"]);
}

/// Catches a listing that holds an unbounded number of jobs: past the most a listing holds it
/// stops, at the cap.
#[tokio::test(start_paused = true)]
async fn a_listing_stops_past_the_most_jobs_it_holds() {
    let rig = Rig::start().await;
    let many = (0..=Client::MAX_LISTED)
        .map(|n| job(&format!("j{n}"), JobState::Running, None))
        .collect();
    rig.fake.inject_on("ListJobs", page(many, "t1"));
    let listed = rig.exec.inner().api().list_live("me").await;
    assert!(
        matches!(listed, Err(Error::TooMany(cap)) if cap == Client::MAX_LISTED),
        "a listing over the cap"
    );
    assert_eq!(tokens(&rig), [""], "no page after the cap");
}
