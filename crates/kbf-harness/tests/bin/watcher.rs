//! A stand-in process for the farm's scratch-order test: `watcher <up> <marker> <saw_gone>`.
//! It writes `<up>`, polls until `<marker>` no longer exists, then writes `<saw_gone>`. A
//! farm that reaps its processes before it removes the directory the marker is in never
//! lets it reach the last write. The poll is a short sleep loop, not a timed assertion.

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [up, marker, saw_gone] = args.as_slice() else {
        eprintln!("usage: watcher <up> <marker> <saw_gone>");
        std::process::exit(2);
    };
    std::fs::write(up, b"").expect("write the up file");
    while std::path::Path::new(marker).exists() {
        std::thread::sleep(Duration::from_micros(200));
    }
    std::fs::write(saw_gone, b"").expect("write the saw_gone file");
}
