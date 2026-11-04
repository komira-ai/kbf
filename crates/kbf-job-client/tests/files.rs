//! A job file's contents (a presigned URL, a token) are secret: no answer of the service
//! carries them, and nothing prints them.

mod support;

use js_proto::v1::{JobFile, ListJobsRequest};
use kbf_job_client::fake::{Call, Fault};
use support::{fake, spec, to_running};

const URL: &str = "https://bucket.example/out.tar?X-Signature=0123456789abcdef";

/// Asserts that a job submitted with a file whose contents are a presigned URL comes back
/// from `submit_job`, `get_job`, `list_jobs` and (as a result) `get_result` with the file's
/// name and empty contents; that the fake's call log holds the contents only in the submit's
/// request, which is what the host would be sent; and that the `Debug` of the spec, the
/// call log, the client and an error of the call do not contain the URL. Catches the fake
/// echoing contents and a `Debug` that prints them into a log.
#[tokio::test]
async fn no_call_leaks_file_contents() {
    let (server, client) = fake().await;
    let mut job = spec("with-file");
    job.files = vec![JobFile {
        name: "upload-url".to_owned(),
        contents: URL.to_owned(),
    }];
    let printed_spec = format!("{job:?}");

    let submitted = client.submit_job(job.clone()).await.unwrap();
    let read = client.get_job("with-file").await.unwrap();
    let listed = client
        .list_jobs(ListJobsRequest::default())
        .await
        .unwrap()
        .jobs;
    for answered in [submitted, read, listed[0].clone()] {
        let files = answered.spec.unwrap().files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "upload-url");
        assert_eq!(files[0].contents, "", "an answer carries no contents");
    }
    to_running(&server, "with-file");
    assert!(server.finish("with-file", 0));
    let result = client.get_result("with-file").await.unwrap();
    assert!(!format!("{result:?}").contains(URL));

    let calls = server.calls();
    let with_contents: Vec<&Call> = calls
        .iter()
        .filter(|call| match call {
            Call::SubmitJob(request) => request.spec.as_ref().unwrap().files[0].contents == URL,
            _ => false,
        })
        .collect();
    assert_eq!(
        with_contents.len(),
        1,
        "only the submit carried the contents"
    );
    assert_eq!(
        server.submitted("with-file").unwrap().files[0].contents,
        URL
    );

    server.inject(Fault::Status {
        code: tonic::Code::Unavailable,
        message: "draining".to_owned(),
    });
    let error = client.submit_job(job).await.unwrap_err();
    let printed = format!("{printed_spec} {calls:?} {client:?} {error:?} {error}");
    assert!(!printed.contains(URL), "{printed}");
    assert!(!printed.contains("X-Signature"), "{printed}");
}
