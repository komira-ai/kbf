//! The fake filer (`filer::fake`) refuses what SeaweedFS 4.54's filer refuses, and lists a
//! directory as it does, so the tests of `FilerPresence` and of the bench over it would
//! fail where the real filer would.

use kbf_objstore::filer::MAX_NAMES_PER_CALL;
use kbf_objstore::filer::fake::FakeFiler;
use kbf_proto::filer_pb::seaweed_filer_client::SeaweedFilerClient;
use kbf_proto::filer_pb::{
    Entry, ListEntriesRequest, LookupDirectoryEntriesRequest, LookupDirectoryEntryRequest,
};
use tonic::Code;
use tonic::transport::Channel;

async fn raw(filer: &FakeFiler) -> SeaweedFilerClient<Channel> {
    SeaweedFilerClient::connect(format!("http://{}", filer.address()))
        .await
        .expect("connect")
}

fn name(directory: &str, name: &str) -> LookupDirectoryEntryRequest {
    LookupDirectoryEntryRequest {
        directory: directory.to_owned(),
        name: name.to_owned(),
    }
}

/// Catches a fake laxer than the filer, under which a client that sends an empty call, one
/// over 4,096 names, or a name the filer refuses would pass its tests: each is refused with
/// the filer's code and text, and still counted as a call.
#[tokio::test]
async fn the_fake_refuses_what_the_filer_refuses() {
    let filer = FakeFiler::start().await;
    let mut client = raw(&filer).await;
    let over: Vec<_> = (0..=MAX_NAMES_PER_CALL)
        .map(|n| name("/d", &n.to_string()))
        .collect();
    let cases = [
        (
            vec![],
            Code::InvalidArgument,
            "batch lookup requires at least one request".to_owned(),
        ),
        (
            over,
            Code::ResourceExhausted,
            format!("batch lookup has 4097 requests; maximum is {MAX_NAMES_PER_CALL}"),
        ),
        (
            vec![name("/d", "a"), name("d", "b")],
            Code::InvalidArgument,
            "batch lookup request 1 has invalid directory".to_owned(),
        ),
        (
            vec![name("/d\0", "a")],
            Code::InvalidArgument,
            "batch lookup request 0 has invalid directory".to_owned(),
        ),
    ];
    let bad_names = ["", ".", "..", "a/b", "a\0b"].map(|bad| {
        (
            vec![name("/d", bad)],
            Code::InvalidArgument,
            "batch lookup request 0 has invalid name".to_owned(),
        )
    });
    let mut counts = Vec::new();
    for (requests, code, message) in cases.into_iter().chain(bad_names) {
        counts.push(requests.len());
        let request = LookupDirectoryEntriesRequest {
            requests,
            unavailable_volume_is_miss: false,
        };
        let status = client
            .lookup_directory_entries(request)
            .await
            .expect_err(&message);
        assert_eq!((status.code(), status.message()), (code, message.as_str()));
    }
    assert_eq!(filer.lookups(), counts);
    assert!(filer.asked().is_empty(), "a refused call reads no name");
}

/// Catches a fake that joins a directory with a trailing `/` into another path than the
/// filer does (`util.JoinPath`): `/d/` and `/d` name the same entry.
#[tokio::test]
async fn the_fake_joins_a_directory_as_the_filer_does() {
    let filer = FakeFiler::start().await;
    filer.put_file("/d/a", 3);
    let mut client = raw(&filer).await;

    let request = LookupDirectoryEntriesRequest {
        requests: vec![name("/d/", "a"), name("/d", "a")],
        unavailable_volume_is_miss: false,
    };
    let answer = client
        .lookup_directory_entries(request)
        .await
        .expect("lookup");

    let found: Vec<bool> = answer
        .into_inner()
        .results
        .iter()
        .map(|r| r.found)
        .collect();
    assert_eq!(found, [true, true]);
}

async fn list(
    client: &mut SeaweedFilerClient<Channel>,
    request: ListEntriesRequest,
) -> Vec<String> {
    let mut stream = client
        .list_entries(request)
        .await
        .expect("list")
        .into_inner();
    let mut names = Vec::new();
    while let Some(listed) = stream.message().await.expect("message") {
        names.push(listed.entry.expect("entry").name);
    }
    names
}

/// Catches a listing that strays out of its directory, or that pages wrongly (a name
/// repeated or skipped between pages): only the entries directly under the directory are
/// listed, by name, from the start name (kept only when asked), at most `limit` (0: all),
/// and only those with the prefix.
#[tokio::test]
async fn the_fake_lists_one_directory_by_name() {
    let filer = FakeFiler::start().await;
    for path in [
        "/r/d/b",
        "/r/d/a",
        "/r/d/c",
        "/r/d/x1",
        "/r/d/sub/deep",
        "/r/e/a",
        "/r/da",
    ] {
        filer.put_file(path, 1);
    }
    let sub = Entry {
        is_directory: true,
        ..Entry::default()
    };
    filer.put_entry("/r/d/sub", sub);
    let mut client = raw(&filer).await;
    let request = |start: &str, inclusive, limit, prefix: &str| ListEntriesRequest {
        directory: "/r/d".to_owned(),
        prefix: prefix.to_owned(),
        start_from_file_name: start.to_owned(),
        inclusive_start_from: inclusive,
        limit,
        ..ListEntriesRequest::default()
    };

    assert_eq!(
        list(&mut client, request("", false, 0, "")).await,
        ["a", "b", "c", "sub", "x1"]
    );
    assert_eq!(
        list(&mut client, request("b", false, 2, "")).await,
        ["c", "sub"]
    );
    assert_eq!(
        list(&mut client, request("b", true, 2, "")).await,
        ["b", "c"]
    );
    assert_eq!(list(&mut client, request("", false, 0, "x")).await, ["x1"]);
    let mut trailing = request("", false, 1, "");
    trailing.directory = "/r/d/".to_owned();
    assert_eq!(list(&mut client, trailing).await, ["a"]);
}
