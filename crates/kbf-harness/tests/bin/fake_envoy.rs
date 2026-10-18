//! A stand-in for Envoy, for the tests of the harness's Envoy path that have no Envoy: run
//! as Envoy is (`--config-path <envoy.json> ...`, the flags after it ignored), it reads the
//! box render, serves `GET /ready` on the admin port (`200 LIVE`), and forwards each TCP
//! connection to the REAPI and worker listeners to the first instance's REAPI and worker
//! ports (TLS passes through untouched), until SIGTERM, which it answers by exiting 0 as
//! Envoy does. It balances nothing and checks no health: the real Envoy is the beta
//! workflow's.

/// The stand-in itself; the library it uses is Linux's.
#[cfg(target_os = "linux")]
mod stand_in {
    use std::io::{Read as _, Write as _};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    use kbf_harness::interrupt::{Interrupt, install};

    /// The port at JSON pointer `at` of the render.
    fn port(doc: &serde_json::Value, at: &str) -> u16 {
        let value = doc.pointer(at).and_then(serde_json::Value::as_u64);
        u16::try_from(value.unwrap_or_else(|| panic!("fake envoy: no port at {at}")))
            .expect("fake envoy: a port")
    }

    /// Answers on `listener` as Envoy's admin page does, in the stages it passes through at
    /// start: `GET /clusters` is answered `503` the first time, with the first instance unhealthy
    /// the second time, and with the lines Envoy lists for it when it is healthy (`clusters`)
    /// after that; any other request is answered `LIVE`.
    fn admin(listener: &TcpListener, clusters: &str) {
        let mut asked = 0;
        for mut stream in listener.incoming().flatten() {
            let mut request = [0_u8; 1024];
            let n = stream.read(&mut request).unwrap_or(0);
            let (code, body) = if request[..n].starts_with(b"GET /clusters ") {
                asked += 1;
                match asked {
                    1 => (503, "starting".to_owned()),
                    2 => (200, clusters.replace("::healthy", "::/failed_active_hc")),
                    _ => (200, clusters.to_owned()),
                }
            } else {
                (200, "LIVE".to_owned())
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    }

    /// Copies `from` to `to` until `from` ends, then ends `to`'s writing side.
    fn copy(mut from: TcpStream, mut to: TcpStream) {
        let _ = std::io::copy(&mut from, &mut to);
        let _ = to.shutdown(Shutdown::Write);
    }

    /// Forwards every connection to `listener` to `127.0.0.1:to`.
    fn forward(listener: &TcpListener, to: u16) {
        for client in listener.incoming().flatten() {
            thread::spawn(move || {
                let Ok(upstream) = TcpStream::connect(("127.0.0.1", to)) else {
                    return;
                };
                let (c, u) = (
                    client.try_clone().expect("clone"),
                    upstream.try_clone().expect("clone"),
                );
                let back = thread::spawn(move || copy(u, c));
                copy(client, upstream);
                let _ = back.join();
            });
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let at = args
            .iter()
            .position(|a| a == "--config-path")
            .expect("--config-path");
        let text = std::fs::read_to_string(&args[at + 1]).expect("the render");
        let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        let instance = |cluster: usize| {
            port(
                &doc,
                &format!(
                    "/static_resources/clusters/{cluster}/load_assignment/endpoints/0/lb_endpoints/0/endpoint/address/socket_address/port_value"
                ),
            )
        };
        let listening = |at: &str| TcpListener::bind(("127.0.0.1", port(&doc, at))).expect("bind");
        let admin_listener = listening("/admin/address/socket_address/port_value");
        let reapi = listening("/static_resources/listeners/0/address/socket_address/port_value");
        let worker = listening("/static_resources/listeners/1/address/socket_address/port_value");
        let (to_reapi, to_worker) = (instance(0), instance(1));
        let clusters = format!(
            "kbf_reapi::127.0.0.1:{to_reapi}::health_flags::healthy\nkbf_workers::127.0.0.1:{to_worker}::health_flags::healthy\n"
        );
        thread::spawn(move || admin(&admin_listener, &clusters));
        thread::spawn(move || forward(&reapi, to_reapi));
        thread::spawn(move || forward(&worker, to_worker));
        // SIGTERM (or SIGINT) ends it with status 0, as Envoy's drain does.
        install();
        let stop = Interrupt::process();
        while !stop.is_set() {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    stand_in::run();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("the stand-in Envoy runs on Linux only");
        std::process::exit(2);
    }
}
