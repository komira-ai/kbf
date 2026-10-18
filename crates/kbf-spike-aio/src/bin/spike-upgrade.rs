//! THROWAWAY SPIKE: how Envoy itself is upgraded under kbf, on a real Envoy.
//!
//! `spike-upgrade <scenario>[,<scenario>] --envoy <old binary> --envoy2 <new binary> --dir <scratch>
//! [--port-base N] [--tag T]`. Scenarios:
//!
//! - `upgrade-<mode>`: (a) cross-version, new Envoy on the same ports (`enable_reuse_port`), old Envoy
//!   drained through the admin; modes `graceful`, `graceful-skip-exit`, `immediate`, `inbound`
//!   (`?inboundonly` with INBOUND listeners), `inbound-undirected`, `early-stop` (the stop rule
//!   without its wait for the drain period, no stream open).
//! - `hrsame` / `hrsecond`: (b) hot restart, one restart / a second restart inside the window.
//! - `hrmismatch-version` / `hrmismatch-size`: (c) hot restart into an Envoy whose `--hot-restart-version`
//!   differs (simulated: no published release differs, see EVIDENCE.md).
//! - `fallback`: (d) stop the old Envoy, start the new one, no overlap.
//! - `overlap-false` / `overlap-default`: a second Envoy on the same ports with `enable_reuse_port`
//!   false / unset.
//!
//! Prints evidence lines, asserts nothing. Time constants are seconds, scaled down from the box's
//! (longest action `L` = 12 s here, `--drain-time-s` 20, `--parent-shutdown-time-s` 25).

use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_proto::grpc::health::v1::{HealthCheckRequest, HealthCheckResponse, HealthListRequest};
use kbf_spike_aio::envoy::{Cfg, Envoy, Flags, http_get, http_post, stat};
use kbf_spike_aio::stub::{Silent, Stub};
use kbf_spike_aio::{ev, now};
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tonic::Streaming;
use tonic::transport::{Channel, Endpoint};

/// The longest action, the drain time and the parent shutdown time, in seconds (the box has
/// hours, 60 s and the longest action + 900 s; only their order is what matters).
const LONGEST: u32 = 12;
const DRAIN: u32 = 20;
const PARENT: u32 = 25;

static PORT: AtomicU16 = AtomicU16::new(31000);

fn ports(n: u16) -> u16 {
    PORT.fetch_add(n, Ordering::SeqCst)
}

struct Env {
    old: PathBuf,
    new: PathBuf,
    dir: PathBuf,
    tag: String,
}

/// The box settings of #925: route timeout 0 and idle_timeout 0 on every route, the connection
/// manager's 7200 s stream idle timeout, breakers raised, `tcp_proxy` idle_timeout 0 and
/// `close_connections_on_host_health_failure` false on the worker cluster, health checks on.
fn box_cfg(admin: u16, http: u16, backend: u16, worker: u16, tcp_backend: u16) -> Cfg {
    let mut c = Cfg::new(admin, http, vec![backend]);
    c.hcm_idle = Some("7200s");
    c.breakers = Some(5000);
    c.reuse_port = Some(true);
    c.tcp = vec![("worker", worker, Some("0s"), false)];
    c.tcp_backends = vec![tcp_backend];
    c
}

async fn launch(env: &Env, bin: &PathBuf, name: &str, cfg: &Cfg, f: &Flags) -> Envoy {
    let conf = env.dir.join(format!("{}-{name}.json", env.tag));
    cfg.write(&conf);
    let log = env.dir.join(format!("{}-{name}.log", env.tag));
    let e = Envoy::start(bin, &conf, &log, cfg.admin, f).await;
    ev!(name, "envoy started (admin {}, pid {:?})", cfg.admin, e.pid);
    e
}

async fn endpoint(port: u16) -> Result<Channel, String> {
    let ep = Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
        .expect("uri")
        .connect_timeout(Duration::from_secs(3));
    match timeout(Duration::from_secs(3), ep.connect()).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(format!("connect: {e}")),
        Err(_) => Err("connect timeout".to_owned()),
    }
}

/// Which stub answers a `List` over `ch`.
async fn who(ch: &Channel) -> Result<String, String> {
    let r = timeout(Duration::from_secs(3), HealthClient::new(ch.clone()).list(HealthListRequest {}))
        .await
        .map_err(|_| "client timeout 3s".to_owned())?
        .map_err(|s| format!("{:?}: {}", s.code(), s.message()))?;
    Ok(r.into_inner().statuses.into_keys().next().unwrap_or_default())
}

async fn wait_serving(port: u16, limit: Duration) -> Option<Channel> {
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if let Ok(ch) = endpoint(port).await {
            if who(&ch).await.is_ok() {
                return Some(ch);
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    None
}

async fn open_watch(ch: &Channel, service: &str) -> Result<Streaming<HealthCheckResponse>, String> {
    let mut s = HealthClient::new(ch.clone())
        .watch(HealthCheckRequest { service: service.to_owned() })
        .await
        .map_err(|s| format!("{:?}: {}", s.code(), s.message()))?
        .into_inner();
    match timeout(Duration::from_secs(10), s.message()).await {
        Ok(Ok(Some(_))) => Ok(s),
        Ok(Ok(None)) => Err("ended before the first frame".to_owned()),
        Ok(Err(e)) => Err(format!("{:?}: {}", e.code(), e.message())),
        Err(_) => Err("no first frame in 10s".to_owned()),
    }
}

/// A held stream: resolves to (time it ended, how).
fn hold(mut s: Streaming<HealthCheckResponse>) -> JoinHandle<(f64, String)> {
    tokio::spawn(async move {
        loop {
            match s.message().await {
                Ok(Some(_)) => {}
                Ok(None) => return (now(), "clean end (OK trailers)".to_owned()),
                Err(e) => return (now(), format!("error {:?}: {}", e.code(), e.message())),
            }
        }
    })
}

type Rows = Arc<Mutex<Vec<(f64, Result<String, String>)>>>;

/// A unary call over a NEW connection every `every_ms`.
fn fresh_prober(port: u16, rows: Rows, run: Arc<AtomicBool>, every_ms: u64) {
    tokio::spawn(async move {
        while run.load(Ordering::SeqCst) {
            let rows = rows.clone();
            tokio::spawn(async move {
                let t = now();
                let r = match endpoint(port).await {
                    Ok(c) => who(&c).await,
                    Err(e) => Err(e),
                };
                rows.lock().expect("lock").push((t, r));
            });
            sleep(Duration::from_millis(every_ms)).await;
        }
    });
}

/// A unary call every `every_ms` over one long-lived channel (which reconnects by itself).
fn persistent_prober(ch: Channel, rows: Rows, run: Arc<AtomicBool>, every_ms: u64) {
    tokio::spawn(async move {
        while run.load(Ordering::SeqCst) {
            let (rows, ch) = (rows.clone(), ch.clone());
            tokio::spawn(async move {
                let t = now();
                let r = who(&ch).await;
                rows.lock().expect("lock").push((t, r));
            });
            sleep(Duration::from_millis(every_ms)).await;
        }
    });
}

/// Calls per interval (`marks` are absolute times; offsets print relative to `rel`).
fn summarize(label: &str, rows: &Rows, marks: &[(&str, f64)], rel: f64) {
    let rows = rows.lock().expect("lock").clone();
    let mut bounds: Vec<(String, f64, f64)> = Vec::new();
    let mut from = ("start".to_owned(), 0.0);
    for (name, t) in marks {
        bounds.push((format!("{}..{name}", from.0), from.1, *t));
        from = ((*name).to_owned(), *t);
    }
    bounds.push((format!("{}..end", from.0), from.1, f64::MAX));
    ev!(label, "{} calls in all, {} errors", rows.len(), rows.iter().filter(|(_, r)| r.is_err()).count());
    for (name, lo, hi) in bounds {
        let sel: Vec<_> = rows.iter().filter(|(t, _)| *t >= lo && *t < hi).collect();
        let a = sel.iter().filter(|(_, r)| r.as_deref() == Ok("a")).count();
        let b = sel.iter().filter(|(_, r)| r.as_deref() == Ok("b")).count();
        let errs: Vec<_> = sel.iter().filter(|(_, r)| r.is_err()).collect();
        ev!(label, "  [{name}] {} calls: on A {a}, on B {b}, errors {}", sel.len(), errs.len());
    }
    let mut errs: Vec<_> = rows.iter().filter(|(_, r)| r.is_err()).collect();
    errs.sort_by(|x, y| x.0.total_cmp(&y.0));
    for (t, r) in errs.iter().take(8) {
        ev!(label, "  error at {:+.2}s: {}", t - rel, r.as_ref().unwrap_err());
    }
}

/// One line per call that landed on `who_id` at or after `from`: how many, the first and last offset.
fn landed_after(label: &str, rows: &Rows, who_id: &str, from: f64) {
    let rows = rows.lock().expect("lock").clone();
    let on: Vec<f64> = rows.iter().filter(|(t, r)| *t >= from && r.as_deref() == Ok(who_id)).map(|(t, _)| t - from).collect();
    let fail: Vec<f64> = rows.iter().filter(|(t, r)| *t >= from && r.is_err()).map(|(t, _)| t - from).collect();
    ev!(
        label,
        "calls answered by {who_id} at or after the mark: {} (first {:?}, last {:?}); failed calls after the mark: {}",
        on.len(),
        on.first().map(|x| (x * 1000.0).round() / 1000.0),
        on.last().map(|x| (x * 1000.0).round() / 1000.0),
        fail.len()
    );
}

fn tail_matching(path: &std::path::Path, needles: &[&str], n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| needles.iter().any(|k| l.to_lowercase().contains(k)))
        .map(|l| l.chars().take(220).collect())
        .collect();
    let skip = v.len().saturating_sub(n);
    v.drain(..skip);
    v
}

// ---------------------------------------------------------------------------------------
// (a) cross-version: reuse_port + drain

/// The daemon-like session: dial the worker listener, read until it is closed, wait 1 s, dial again.
/// Each row: (connected at, which Envoy counted it, ended at).
type Sessions = Arc<Mutex<Vec<(f64, String, Option<f64>)>>>;

fn daemon(worker: u16, admin_a: u16, admin_b: u16, rows: Sessions, run: Arc<AtomicBool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let stat_of = |admin| async move { stat(admin, "tcp.worker.downstream_cx_total").await.unwrap_or(0) };
        while run.load(Ordering::SeqCst) {
            let (ca, cb, t) = (stat_of(admin_a).await, stat_of(admin_b).await, now());
            let mut c = match tokio::net::TcpStream::connect(("127.0.0.1", worker)).await {
                Ok(c) => c,
                Err(e) => {
                    rows.lock().expect("lock").push((t, format!("connect error {e}"), Some(now())));
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            sleep(Duration::from_millis(300)).await;
            let on = if stat_of(admin_a).await > ca {
                "A"
            } else if stat_of(admin_b).await > cb {
                "B"
            } else {
                "?"
            };
            let idx = {
                let mut r = rows.lock().expect("lock");
                r.push((t, on.to_owned(), None));
                r.len() - 1
            };
            let mut buf = [0u8; 64];
            loop {
                match c.read(&mut buf).await {
                    Ok(n) if n > 0 => {}
                    _ => break,
                }
            }
            rows.lock().expect("lock")[idx].2 = Some(now());
            sleep(Duration::from_secs(1)).await;
        }
    })
}

async fn upgrade(env: &Env, mode: &str) {
    let (path, direction) = match mode {
        "graceful" | "early-stop" => ("/drain_listeners?graceful", None),
        "graceful-skip-exit" => ("/drain_listeners?graceful&skip_exit", None),
        "immediate" => ("/drain_listeners", None),
        "inbound" => ("/drain_listeners?graceful&inboundonly&skip_exit", Some("INBOUND")),
        "inbound-undirected" => ("/drain_listeners?graceful&inboundonly&skip_exit", None),
        other => panic!("unknown mode {other}"),
    };
    let lab = format!("upgrade[{mode}]");
    let p = ports(8);
    let (admin_a, admin_b, http, worker) = (p, p + 1, p + 2, p + 3);
    let (sa, sb) = (Stub::start("a", p + 4), Stub::start("b", p + 5));
    let _silent = Silent::start(p + 6);
    let mut cfg_a = box_cfg(admin_a, http, p + 4, worker, p + 6);
    let mut cfg_b = box_cfg(admin_b, http, p + 5, worker, p + 6);
    cfg_a.direction = direction;
    cfg_b.direction = direction;
    let base = 700 + (p % 200) as u32;
    let fa = Flags {
        base_id: Some(base),
        epoch: 0,
        parent_shutdown_s: Some(PARENT),
        drain_s: Some(DRAIN),
        drain_strategy: Some("gradual"),
    };
    let fb = Flags { base_id: Some(base + 1), ..fa.clone() };
    let mut ea = launch(env, &env.old, "A", &cfg_a, &fa).await;
    ea.wait_ready(Duration::from_secs(10)).await;
    let Some(chs) = wait_serving(http, Duration::from_secs(10)).await else {
        return ev!(lab, "A never served");
    };
    let mut streams = Vec::new();
    // `early-stop` has no stream, so the HTTP side is idle and the stop rule fires at once.
    let early = mode == "early-stop";
    for svc in ["short", "long", "over"].into_iter().filter(|_| !early) {
        let s = open_watch(&chs, svc).await.expect("watch");
        streams.push((svc, now(), hold(s)));
    }
    let run = Arc::new(AtomicBool::new(true));
    let fresh: Rows = Arc::default();
    let pers: Rows = Arc::default();
    let sessions: Sessions = Arc::default();
    persistent_prober(chs.clone(), pers.clone(), run.clone(), 50);
    fresh_prober(http, fresh.clone(), run.clone(), 25);
    let dtask = daemon(worker, admin_a, admin_b, sessions.clone(), run.clone());
    sleep(Duration::from_secs(3)).await;

    // New Envoy on the same ports.
    let t_b_spawn = now();
    let mut eb = launch(env, &env.new, "B", &cfg_b, &fb).await;
    let ready = eb.wait_ready(Duration::from_secs(20)).await;
    let t_b_ready = now();
    ev!(lab, "B ready={ready} {:.2}s after spawn; both listen on port {http} (reuse_port)", t_b_ready - t_b_spawn);
    sleep(Duration::from_secs(4)).await;

    // Drain A.
    let a_cx0 = stat(admin_a, "http.reapi.downstream_cx_total").await;
    let b_cx0 = stat(admin_b, "http.reapi.downstream_cx_total").await;
    let t_drain = now();
    let r = http_post(admin_a, path).await;
    ev!(lab, "POST {path} on A -> {r:?}");
    {
        let sa = sa.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(6)).await;
            sa.end_service("short");
            sleep(Duration::from_secs(u64::from(LONGEST) - 6)).await;
            sa.end_service("long");
            sleep(Duration::from_secs(u64::from(DRAIN) + 8 - u64::from(LONGEST))).await;
            sa.end_service("over");
        });
    }
    let mut term_at = None;
    let mut idle_since: Option<f64> = None;
    let mut last_log = -10.0;
    let mut listed = false;
    loop {
        sleep(Duration::from_millis(250)).await;
        let off = now() - t_drain;
        let act = stat(admin_a, "http.reapi.downstream_cx_active").await;
        if off - last_log >= 2.0 {
            last_log = off;
            let tot = stat(admin_a, "server.total_connections").await;
            let drn = stat(admin_a, "listener_manager.total_listeners_draining").await;
            let stopped = stat(admin_a, "listener_manager.total_listeners_stopped").await;
            ev!(lab, "A +{off:.1}s: http cx active {act:?}, total connections {tot:?}, listeners draining {drn:?}, stopped {stopped:?}");
        }
        if off >= f64::from(DRAIN) + 2.0 && !listed {
            listed = true;
            let l = http_get(admin_a, "/listeners").await.map(|(_, b)| b.replace('\n', " | "));
            ev!(lab, "A /listeners at +{off:.1}s: {l:?}");
        }
        if ea.exit().is_some() || off > f64::from(DRAIN) + 40.0 {
            break;
        }
        // The installer's stop rule: the drain period is over (the listeners are closed) and
        // the HTTP side has been idle for 1 s, then SIGTERM. `early-stop` drops the first half.
        if act == Some(0) && term_at.is_none() && (early || off >= f64::from(DRAIN) + 1.0) {
            let since = *idle_since.get_or_insert(off);
            if off - since >= 1.0 {
                term_at = Some(off);
                ev!(lab, "A has no HTTP connection for 1s at +{off:.1}s: SIGTERM");
                ea.term().await;
            }
        } else if act != Some(0) {
            idle_since = None;
        }
    }
    let a_exit = ea.exit();
    ev!(lab, "A exit: {:?} (drain began at t={t_drain:.2}s; SIGTERM sent at {:?})", a_exit.as_ref().map(|(t, s)| (format!("+{:.2}s", t - t_drain), s.clone())), term_at);
    let t_a_exit = a_exit.as_ref().map_or(f64::MAX, |(t, _)| *t);
    let a_cx1 = stat(admin_a, "http.reapi.downstream_cx_total").await;
    let b_cx1 = stat(admin_b, "http.reapi.downstream_cx_total").await;
    sleep(Duration::from_secs(6)).await;
    run.store(false, Ordering::SeqCst);
    sa.end_watches();
    for svc in ["short", "long", "over"] {
        sa.end_service(svc);
    }
    sleep(Duration::from_secs(4)).await;
    ev!(lab, "A http connections accepted: before drain {a_cx0:?}, at exit {a_cx1:?}; B: before {b_cx0:?}, now {b_cx1:?}");
    for (svc, t_open, h) in streams {
        let (t_end, how) = h.await.expect("join");
        ev!(lab, "stream {svc}: opened t={t_open:.2}s, ended drain{:+.2}s ({:.2}s after open): {how}", t_end - t_drain, t_end - t_open);
    }
    ev!(lab, "stub a: watches {}, cancelled by the peer {}, open now {}", sa.watches.load(Ordering::SeqCst), sa.cancelled.load(Ordering::SeqCst), sa.open.load(Ordering::SeqCst));
    ev!(lab, "stub b: watches {}", sb.watches.load(Ordering::SeqCst));
    let marks = [("B-ready", t_b_ready), ("drain", t_drain), ("A-exit", t_a_exit)];
    ev!(lab, "marks (t): B-ready {t_b_ready:.2}, drain {t_drain:.2}, A-exit {:.2}", t_a_exit);
    summarize(&format!("{lab} fresh-connection calls"), &fresh, &marks, t_drain);
    landed_after(&format!("{lab} fresh-connection calls"), &fresh, "a", t_drain);
    summarize(&format!("{lab} one-channel calls"), &pers, &marks, t_drain);
    landed_after(&format!("{lab} one-channel calls"), &pers, "a", t_drain);
    dtask.abort();
    for (t, on, end) in sessions.lock().expect("lock").iter() {
        ev!(lab, "daemon session: connected drain{:+.2}s on {on}, ended {}", t - t_drain, end.map_or("still open at the end".to_owned(), |e| format!("drain{:+.2}s", e - t_drain)));
    }
    ea.kill();
    eb.kill();
}

// ---------------------------------------------------------------------------------------
// (d) fallback: no overlap

async fn fallback(env: &Env) {
    let lab = "fallback";
    let p = ports(8);
    let (admin_a, admin_b, http, worker) = (p, p + 1, p + 2, p + 3);
    let (_sa, _sb) = (Stub::start("a", p + 4), Stub::start("b", p + 5));
    let _silent = Silent::start(p + 6);
    let cfg_a = box_cfg(admin_a, http, p + 4, worker, p + 6);
    let cfg_b = box_cfg(admin_b, http, p + 5, worker, p + 6);
    let base = 700 + (p % 200) as u32;
    let fa = Flags { base_id: Some(base), epoch: 0, parent_shutdown_s: Some(PARENT), drain_s: Some(DRAIN), drain_strategy: Some("gradual") };
    let fb = Flags { base_id: Some(base + 1), ..fa.clone() };
    let ea = launch(env, &env.old, "A", &cfg_a, &fa).await;
    ea.wait_ready(Duration::from_secs(10)).await;
    let Some(chs) = wait_serving(http, Duration::from_secs(10)).await else {
        return ev!(lab, "A never served");
    };
    let run = Arc::new(AtomicBool::new(true));
    let fresh: Rows = Arc::default();
    let pers: Rows = Arc::default();
    persistent_prober(chs, pers.clone(), run.clone(), 50);
    fresh_prober(http, fresh.clone(), run.clone(), 25);
    sleep(Duration::from_secs(3)).await;
    let t_term = now();
    ea.term().await;
    let eb = launch(env, &env.new, "B", &cfg_b, &fb).await;
    let t_spawn = now();
    let ready = eb.wait_ready(Duration::from_secs(20)).await;
    let t_ready = now();
    ev!(lab, "SIGTERM to A at t={t_term:.2}s; A exit {:?}; B spawned +{:.3}s, /ready={ready} at +{:.3}s", ea.exit().map(|(t, s)| (format!("+{:.3}s", t - t_term), s)), t_spawn - t_term, t_ready - t_term);
    sleep(Duration::from_secs(5)).await;
    run.store(false, Ordering::SeqCst);
    sleep(Duration::from_secs(4)).await;
    ev!(lab, "A exit {:?}", ea.exit().map(|(t, s)| (format!("+{:.3}s", t - t_term), s)));
    summarize("fallback fresh-connection calls", &fresh, &[("SIGTERM", t_term), ("B-ready", t_ready)], t_term);
    let rows = fresh.lock().expect("lock").clone();
    let first_err = rows.iter().filter(|(t, r)| *t >= t_term && r.is_err()).map(|(t, _)| *t).fold(f64::MAX, f64::min);
    let last_err = rows.iter().filter(|(t, r)| *t >= t_term && r.is_err()).map(|(t, _)| *t).fold(0.0, f64::max);
    let first_ok_b = rows.iter().filter(|(t, r)| *t >= t_term && r.as_deref() == Ok("b")).map(|(t, _)| *t).fold(f64::MAX, f64::min);
    ev!(lab, "new connections: first failed call at {:+.3}s, last failed call at {:+.3}s, first call answered by B at {:+.3}s", first_err - t_term, last_err - t_term, first_ok_b - t_term);
    summarize("fallback one-channel calls", &pers, &[("SIGTERM", t_term), ("B-ready", t_ready)], t_term);
    eb.wait_ready(Duration::from_millis(1)).await;
    drop(eb);
}

/// A second Envoy on the first one's ports with `enable_reuse_port` set to `reuse`.
async fn overlap(env: &Env, reuse: Option<bool>) {
    let lab = format!("overlap[{reuse:?}]");
    let p = ports(8);
    let (admin_a, admin_b, http, worker) = (p, p + 1, p + 2, p + 3);
    let (_sa, _sb) = (Stub::start("a", p + 4), Stub::start("b", p + 5));
    let _silent = Silent::start(p + 6);
    let mut cfg_a = box_cfg(admin_a, http, p + 4, worker, p + 6);
    let mut cfg_b = box_cfg(admin_b, http, p + 5, worker, p + 6);
    cfg_a.reuse_port = reuse;
    cfg_b.reuse_port = reuse;
    let base = 700 + (p % 200) as u32;
    let fa = Flags { base_id: Some(base), epoch: 0, parent_shutdown_s: Some(PARENT), drain_s: Some(DRAIN), drain_strategy: Some("gradual") };
    let fb = Flags { base_id: Some(base + 1), ..fa.clone() };
    let ea = launch(env, &env.old, "A", &cfg_a, &fa).await;
    ea.wait_ready(Duration::from_secs(10)).await;
    wait_serving(http, Duration::from_secs(10)).await;
    let t = now();
    let eb = launch(env, &env.new, "B", &cfg_b, &fb).await;
    let ready = eb.wait_ready(Duration::from_secs(8)).await;
    sleep(Duration::from_secs(1)).await;
    ev!(lab, "B /ready={ready}, exit {:?}", eb.exit().map(|(x, s)| (format!("+{:.2}s", x - t), s)));
    for l in tail_matching(&eb.log, &["address", "error", "critical", "fail"], 4) {
        ev!(lab, "B log: {l}");
    }
    if ready {
        let rows: Rows = Arc::default();
        let run = Arc::new(AtomicBool::new(true));
        fresh_prober(http, rows.clone(), run.clone(), 10);
        sleep(Duration::from_secs(8)).await;
        run.store(false, Ordering::SeqCst);
        sleep(Duration::from_secs(3)).await;
        summarize(&format!("{lab} fresh-connection calls"), &rows, &[], t);
    }
    drop((ea, eb));
}

// ---------------------------------------------------------------------------------------
// (b) hot restart

async fn wait_epoch(admin: u16, epoch: u32, limit: Duration) -> bool {
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if let Some((200, b)) = http_get(admin, "/server_info").await {
            let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
            if v["command_line_options"]["restart_epoch"].as_u64() == Some(u64::from(epoch)) && v["state"] == "LIVE" {
                return true;
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    false
}

struct Hr {
    p: u16,
    cfg: Cfg,
    f0: Flags,
    _stub: Arc<Stub>,
    stub: Arc<Stub>,
    _silent: Arc<Silent>,
}

fn hr_setup() -> Hr {
    let p = ports(8);
    let stub = Stub::start("a", p + 2);
    let silent = Silent::start(p + 4);
    let cfg = box_cfg(p, p + 1, p + 2, p + 3, p + 4);
    let base = 700 + (p % 200) as u32;
    let f0 = Flags { base_id: Some(base), epoch: 0, parent_shutdown_s: Some(PARENT), drain_s: Some(DRAIN / 2), drain_strategy: Some("gradual") };
    Hr { p, cfg, f0, _stub: stub.clone(), stub, _silent: silent }
}

async fn hrsame(env: &Env, second: bool) {
    let lab = if second { "hrsecond" } else { "hrsame" };
    let hr = hr_setup();
    let (p, http) = (hr.p, hr.p + 1);
    let e0 = launch(env, &env.old, "e0", &hr.cfg, &hr.f0).await;
    e0.wait_ready(Duration::from_secs(10)).await;
    let Some(ch) = wait_serving(http, Duration::from_secs(10)).await else {
        return ev!(lab, "epoch 0 never served");
    };
    let mut streams = Vec::new();
    for svc in ["short", "over"] {
        streams.push((svc, "epoch 0", hold(open_watch(&ch, svc).await.expect("watch"))));
    }
    let tcp = {
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", p + 3)).await.expect("tcp");
        tokio::spawn(async move {
            let mut buf = [0u8; 8];
            let r = c.read(&mut buf).await;
            (now(), format!("{r:?}"))
        })
    };
    let run = Arc::new(AtomicBool::new(true));
    let (fresh, pers): (Rows, Rows) = (Arc::default(), Arc::default());
    persistent_prober(ch.clone(), pers.clone(), run.clone(), 50);
    fresh_prober(http, fresh.clone(), run.clone(), 25);
    sleep(Duration::from_secs(3)).await;
    let t_r1 = now();
    let e1 = launch(env, &env.new, "e1", &hr.cfg, &Flags { epoch: 1, ..hr.f0.clone() }).await;
    let up1 = wait_epoch(p, 1, Duration::from_secs(20)).await;
    let t_up1 = now();
    ev!(lab, "restart 1 (epoch 1) spawned t={t_r1:.2}s, LIVE as epoch 1: {up1} at +{:.2}s", t_up1 - t_r1);
    {
        let stub = hr.stub.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(8)).await;
            stub.end_service("short");
        });
    }
    let (mut t_r2, mut t_up2, mut e2) = (None, None, None);
    if second {
        sleep(Duration::from_secs(3)).await;
        let ch1 = endpoint(http).await.expect("ch1");
        streams.push(("epoch1-stream", "epoch 1", hold(open_watch(&ch1, "epoch1").await.expect("watch"))));
        sleep(Duration::from_secs(1)).await;
        let t = now();
        let e = launch(env, &env.new, "e2", &hr.cfg, &Flags { epoch: 2, ..hr.f0.clone() }).await;
        let up = wait_epoch(p, 2, Duration::from_secs(20)).await;
        let tu = now();
        ev!(lab, "restart 2 (epoch 2) spawned {:+.2}s after restart 1 (inside the {PARENT}s window), LIVE as epoch 2: {up} at +{:.3}s", t - t_r1, tu - t);
        (t_r2, t_up2, e2) = (Some(t), Some(tu), Some(e));
    }
    let until = t_r2.unwrap_or(t_r1) + f64::from(PARENT) + 8.0;
    while now() < until {
        sleep(Duration::from_millis(250)).await;
    }
    run.store(false, Ordering::SeqCst);
    hr.stub.end_service("over");
    hr.stub.end_service("epoch1");
    sleep(Duration::from_secs(3)).await;
    ev!(lab, "marks: restart1 spawn t={t_r1:.3}, LIVE t={t_up1:.3}; restart2 spawn {t_r2:?}, LIVE {t_up2:?}");
    for (svc, on, h) in streams {
        let (t_end, how) = h.await.expect("join");
        let r2 = t_r2.map_or(String::new(), |t| format!(", restart2 spawn{:+.3}s, restart2 LIVE{:+.3}s", t_end - t, t_end - t_up2.unwrap_or(t)));
        ev!(lab, "stream {svc} (opened on {on}): ended restart1 spawn{:+.2}s, restart1 LIVE{:+.2}s{r2}: {how}", t_end - t_r1, t_end - t_up1);
    }
    let (t_tcp, r) = tcp.await.expect("join");
    ev!(lab, "worker tcp_proxy session (opened on epoch 0): ended restart1 spawn{:+.2}s: {r}", t_tcp - t_r1);
    ev!(lab, "exits: e0 {:?}, e1 {:?}, e2 {:?}", e0.exit(), e1.exit(), e2.as_ref().and_then(Envoy::exit));
    let marks: Vec<(&str, f64)> = [Some(("restart1", t_r1)), t_r2.map(|t| ("restart2", t))].into_iter().flatten().collect();
    summarize(&format!("{lab} fresh-connection calls"), &fresh, &marks, t_r1);
    summarize(&format!("{lab} one-channel calls"), &pers, &marks, t_r1);
    let (mut e0, mut e1) = (e0, e1);
    e0.kill();
    e1.kill();
    if let Some(mut e) = e2 {
        e.kill();
    }
}

// ---------------------------------------------------------------------------------------
// (c) hot restart into an Envoy with another hot restart version (simulated in shared memory)

async fn hrmismatch(env: &Env, what: &str) {
    let lab = format!("hrmismatch[{what}]");
    let hr = hr_setup();
    let (p, http) = (hr.p, hr.p + 1);
    let mut e0 = launch(env, &env.old, "e0", &hr.cfg, &hr.f0).await;
    e0.wait_ready(Duration::from_secs(10)).await;
    let Some(ch) = wait_serving(http, Duration::from_secs(10)).await else {
        return ev!(lab, "epoch 0 never served");
    };
    let held = hold(open_watch(&ch, "long").await.expect("watch"));
    let base = hr.f0.base_id.expect("base");
    let shm = format!("/dev/shm/envoy_shared_memory_{}", base * 10);
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&shm).expect("shm");
    let mut hdr = [0u8; 16];
    f.read_exact_at(&mut hdr, 0).expect("read shm");
    let (size, version) = (u64::from_ne_bytes(hdr[..8].try_into().expect("8")), u64::from_ne_bytes(hdr[8..].try_into().expect("8")));
    ev!(lab, "shared memory {shm}: size_ {size}, version_ {version} (the --hot-restart-version of this binary is {version}.{size})");
    let (off, new) = if what == "size" { (0, size + 8) } else { (8, version + 1) };
    f.write_all_at(&new.to_ne_bytes(), off).expect("poke");
    ev!(lab, "poked {} to {new}: epoch 0's shared memory now says what an Envoy of another hot restart version would have written", if what == "size" { "size_" } else { "version_" });
    let t = now();
    let e1 = launch(env, &env.new, "e1", &hr.cfg, &Flags { epoch: 1, ..hr.f0.clone() }).await;
    let end = tokio::time::Instant::now() + Duration::from_secs(15);
    while e1.exit().is_none() && tokio::time::Instant::now() < end {
        sleep(Duration::from_millis(20)).await;
    }
    ev!(lab, "epoch 1 exit: {:?}", e1.exit().map(|(x, s)| (format!("+{:.3}s after spawn", x - t), s)));
    for l in tail_matching(&e1.log, &["assert failure"], 2) {
        ev!(lab, "epoch 1 log: {l}");
    }
    sleep(Duration::from_secs(1)).await;
    let info = http_get(p, "/server_info").await.map(|(_, b)| {
        let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
        format!("state={} epoch={}", v["state"], v["command_line_options"]["restart_epoch"])
    });
    let r = match endpoint(http).await {
        Ok(c) => who(&c).await,
        Err(e) => Err(e),
    };
    ev!(lab, "epoch 0 after the failed attempt: admin {info:?}; fresh call {r:?}; exit {:?}; held stream finished: {}", e0.exit(), held.is_finished());
    hr.stub.end_service("long");
    let (_, how) = held.await.expect("join");
    ev!(lab, "held stream (opened on epoch 0): {how}");
    e0.kill();
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let old = PathBuf::from(get("--envoy").expect("--envoy <old binary>"));
    let new = get("--envoy2").map_or_else(|| old.clone(), PathBuf::from);
    let env = Env { old, new, dir: PathBuf::from(get("--dir").expect("--dir")), tag: get("--tag").unwrap_or_else(|| "run".into()) };
    if let Some(b) = get("--port-base") {
        PORT.store(b.parse().expect("port base"), Ordering::SeqCst);
    }
    assert!(env.old.exists() && env.new.exists());
    let _ = now();
    for scenario in args.get(1).expect("scenario").split(',') {
        ev!("scenario", "== {scenario} (old {}, new {}) ==", env.old.display(), env.new.display());
        match scenario {
            s if s.starts_with("upgrade-") => upgrade(&env, &s["upgrade-".len()..]).await,
            "hrsame" => hrsame(&env, false).await,
            "hrsecond" => hrsame(&env, true).await,
            "hrmismatch-version" => hrmismatch(&env, "version").await,
            "hrmismatch-size" => hrmismatch(&env, "size").await,
            "fallback" => fallback(&env).await,
            "overlap-false" => overlap(&env, Some(false)).await,
            "overlap-default" => overlap(&env, None).await,
            other => panic!("unknown scenario {other}"),
        }
    }
}
