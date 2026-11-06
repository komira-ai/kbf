//! A job file's contents (a presigned URL, a token) are secret: no answer of the service
//! carries them, and nothing prints them.

mod support;

use js_proto::v1::{JobFile, ListJobsRequest};
use kbf_job_client::fake::{Call, Fault};
use support::{fake, spec, to_running};

const URL: &str = "https://bucket.example/out.tar?X-Signature=0123456789abcdef";
/// A secret of the same length as [`URL`] whose signature differs at every position.
const OTHER: &str = "https://bucket.example/out.tar?X-Signature=fedcba9876543210";

/// Everything a run of [`scenario`] prints.
struct Printed {
    spec: String,
    calls: String,
    error: String,
    message: String,
    result: String,
    client: String,
    origin: String,
}

/// Submits a job with a file whose contents are `secret`, reads it back every way, and has a
/// later submit fail; checks what the answers and the call log hold, and returns what each
/// `Debug` printed.
async fn scenario(secret: &str) -> Printed {
    let (server, client) = fake().await;
    let mut job = spec("with-file");
    job.files = vec![JobFile {
        name: "upload-url".to_owned(),
        contents: secret.to_owned(),
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
    let logs = client.get_logs("with-file").await.unwrap();
    let printed_result = format!("{result:?} {logs:?}");

    let carrying: Vec<Call> = server
        .calls()
        .into_iter()
        .filter(|call| match call {
            Call::SubmitJob(request) => request.spec.as_ref().unwrap().files[0].contents == secret,
            _ => false,
        })
        .collect();
    assert_eq!(carrying.len(), 1, "only the submit carried the contents");
    assert_eq!(
        server.submitted("with-file").unwrap().files[0].contents,
        secret
    );

    server.inject(Fault::Status {
        code: tonic::Code::Unavailable,
        message: "draining".to_owned(),
    });
    let error = client.submit_job(job).await.unwrap_err();
    Printed {
        spec: printed_spec,
        calls: format!("{:?}", server.calls()),
        error: format!("{error:?}"),
        message: error.to_string(),
        result: printed_result,
        client: format!("{client:?}"),
        origin: server.origin(),
    }
}

/// Asserts that a job submitted with a file whose contents are a presigned URL comes back
/// from `submit_job`, `get_job`, `list_jobs` and (as a result) `get_result` with the file's
/// name and empty contents, and that the fake's call log holds the contents only in the
/// submit's request, which is what the host would be sent; and that what the `Debug` of the
/// spec, of the call log, of the results and of an error of the call print is exactly the
/// same when the URL is another of the same length, so no representation of the contents is
/// in it, and the `Debug` of a file is its name and the length of its contents. Catches the
/// fake echoing contents and a `Debug` that prints them into a log, in any form.
#[tokio::test]
async fn no_call_leaks_file_contents() {
    assert_eq!(URL.len(), OTHER.len());
    assert_ne!(URL, OTHER);
    let (one, other) = (scenario(URL).await, scenario(OTHER).await);
    assert_eq!(one.spec, other.spec);
    assert_eq!(one.calls, other.calls);
    assert_eq!(one.error, other.error);
    assert_eq!(one.message, other.message);
    assert_eq!(one.result, other.result);

    let authority = one.origin.trim_start_matches("http://");
    assert_eq!(
        one.client,
        format!(r#"Client {{ origin: "{authority}", tls: false, .. }}"#)
    );
    let file = JobFile {
        name: "upload-url".to_owned(),
        contents: URL.to_owned(),
    };
    assert_eq!(
        format!("{file:?}"),
        format!(
            r#"JobFile {{ name: "upload-url", contents_len: {}, .. }}"#,
            URL.len()
        )
    );
}
