//! The generated `JobService` over the fake's [`State`](super::State): each call is logged
//! with its request, then answered by the state's rules. The calls the client does not wrap
//! (hosts, pools, providers, usage) keep the generated default, `UNIMPLEMENTED`.

use std::sync::Arc;

use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, GetJobRequest, GetJobResponse, GetLogsRequest,
    GetLogsResponse, GetResultRequest, GetResultResponse, ListJobsRequest, ListJobsResponse,
    SubmitJobRequest, SubmitJobResponse,
};
use tonic::{Request, Response, Status};

use super::{Call, Shared};

/// The service of one fake.
pub(super) struct Service {
    pub(super) shared: Arc<Shared>,
}

#[tonic::async_trait]
impl JobService for Service {
    async fn submit_job(
        &self,
        request: Request<SubmitJobRequest>,
    ) -> Result<Response<SubmitJobResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .record(Call::SubmitJob(Box::new(request.clone())));
        let spec = request.spec.unwrap_or_default();
        let job = self.shared.state().submit(spec)?;
        Ok(Response::new(SubmitJobResponse { job: Some(job) }))
    }

    async fn get_job(
        &self,
        request: Request<GetJobRequest>,
    ) -> Result<Response<GetJobResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::GetJob(request.clone()));
        let job = self.shared.state().get(&request.job_id)?;
        Ok(Response::new(GetJobResponse { job: Some(job) }))
    }

    async fn list_jobs(
        &self,
        request: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::ListJobs(request.clone()));
        let page = self.shared.state().list(&request)?;
        Ok(Response::new(page))
    }

    async fn cancel_job(
        &self,
        request: Request<CancelJobRequest>,
    ) -> Result<Response<CancelJobResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::CancelJob(request.clone()));
        let job = self.shared.state().cancel(&request.job_id)?;
        Ok(Response::new(CancelJobResponse { job: Some(job) }))
    }

    async fn get_result(
        &self,
        request: Request<GetResultRequest>,
    ) -> Result<Response<GetResultResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::GetResult(request.clone()));
        let result = self.shared.state().result(&request.job_id)?;
        Ok(Response::new(GetResultResponse {
            result: Some(result),
        }))
    }

    async fn get_logs(
        &self,
        request: Request<GetLogsRequest>,
    ) -> Result<Response<GetLogsResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::GetLogs(request.clone()));
        let logs = self.shared.state().logs(&request.job_id)?;
        Ok(Response::new(logs))
    }
}
