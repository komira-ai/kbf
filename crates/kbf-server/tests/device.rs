//! iOS device leases end to end (`docs/design/ios-devices.md`, section 5.4): a daemon
//! reports its devices in `Hello`, an action asks for one with `ios.device=1`, and the
//! `Start` the server sends names the booked device in `device_id`.

mod support;

use support::{Cell, FakeDaemon, Job, done, hello_on, output, ran};

const MAC: [(&str, &str); 2] = [("arch", "arm64"), ("os", "macos")];

/// Catches: a `Start` that does not carry the device the scheduler booked (the daemon
/// would run the device test against no device, or the wrong one), and a device lease
/// sent to a daemon that reports no device: `node-a` sorts first and reports none, as
/// a daemon that predates devices does, and would drop `device_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_of_a_device_lease_names_its_device() {
    let cell = Cell::start().await;
    let mut old = FakeDaemon::connect(cell.worker_addr, hello_on("node-a", 4, 8, &MAC))
        .await
        .expect("registered");
    let phone =
        "class=iPhone,id=00008140-0001,os_build=23A341,os_version=26.0,product_type=iPhone17,3";
    let with_device = [MAC[0], MAC[1], ("ios.device", phone)];
    let mut new = FakeDaemon::connect(cell.worker_addr, hello_on("node-b", 4, 8, &with_device))
        .await
        .expect("registered");

    let device_job = Job::new(
        "on a phone",
        &[
            ("OSFamily", "Darwin"),
            ("ios.device", "1"),
            ("ios.device.class", "iPhone"),
        ],
    );
    let plain_job = Job::new("anywhere", &[]);
    cell.upload(&device_job.blobs()).await;
    cell.upload(&plain_job.blobs()).await;

    let mut ops = cell.execute(&device_job.action).await;
    let start = new.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&device_job.action.proto));
    assert_eq!(start.device_id, "00008140-0001");
    old.no_work().await;
    let tested = output(&cell, "tested on the phone", 0).await;
    assert!(new.report(ran(start.lease_id, &tested)).await.accepted);
    done(&mut ops).await;

    // An action that asks for no device gets a Start with no device.
    let mut ops = cell.execute(&plain_job.action).await;
    let start = old.start().await;
    assert_eq!(start.device_id, "");
    let built = output(&cell, "built", 0).await;
    assert!(old.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;
}
