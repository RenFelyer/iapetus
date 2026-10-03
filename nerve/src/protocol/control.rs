//! Сообщения канала управления, по ролям: вход узла, worker, runner, session. Набор запросов задан ролью
//! соединения; ответы описаны у запроса.

mod enroll;
mod listing;
mod runner;
mod session;
mod worker;

pub use enroll::{
    EnrollRequest, RENEW_NAMESPACE, RegisterReply, RegisterRequest, RegisterResponse, RenewReply, RenewRequest, RenewResponse,
};
pub use listing::{NodeInfo, RunnerInfo, RunnerPresence, SessionInfo};
pub use runner::{Assignment, Author, CloneReply, CloneRequest, CloneResponse, RunnerRequest, SessionMessage, SessionResponse, ToRunner};
pub use session::{
    Activity, AgentRelease, AgentRun, Artifact, ArtifactFormat, AssignedRunner, Cause, Control, ControlResponse, Digest, DocsEvent,
    Document, Platform, SessionRequest, Status, UsageWindow,
};
pub use worker::{
    EnvChange, EnvChangeResponse, EnvEntry, EnvEvent, EnvOp, NodesOp, RunnersOp, SessionEvent, SessionIdentity, SessionsOp,
    WorkerCertificateReply, WorkerCertificateRequest, WorkerCertificateResponse, WorkerCloneRequest, WorkerRequest,
};
