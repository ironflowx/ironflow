//! What the steps of `events.feature` and `tenancy.feature` call: an engine in the test's process,
//! or the `ironflowx` binary through the proto client.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cucumber::gherkin::Scenario;
use ironflowx_operations::Engine;
use ironflowx_proto::{
    Call, Client, CreateKeyRequest, CreatedKey, EmitBatchRequest, EmitBatchResponse, EmitRequest,
    EmitResponse, Environment, Event, EventPage, GetEventRequest, KeyPage, ListEnvironmentsRequest,
    ListEventsRequest, ListKeysRequest, OpError, Operations, RevokeKeyRequest, Whoami,
    WhoamiRequest,
};

use super::output::Audit;
use super::serve::Served;

/// How many scenarios run at once against the binary, each with one of its own to start.
pub const BINARIES: usize = 8;

/// Whether a scenario runs against the binary: not one tagged `@local`.
pub fn runs_remotely(scenario: &Scenario) -> bool {
    !scenario.tags.iter().any(|tag| tag == "local")
}

pub enum Adapter {
    Engine(Engine),
    Binary(Box<Remote>),
}

pub struct Remote {
    client: Client,
    served: Served,
    data_dir: PathBuf,
    audit: Mutex<Audit>,
}

impl Remote {
    fn presented(&self, call: &Call) {
        if let Some(key) = call.key() {
            self.audit.lock().unwrap().presented(key);
        }
    }

    fn diagnostic(&self, error: &OpError) {
        self.audit.lock().unwrap().op_error(error);
    }
}

impl Adapter {
    /// Serves a fresh data directory: the adapter, and the key in `admin.key`.
    pub async fn served(data_dir: &Path) -> (Self, String) {
        let key_file = data_dir.join("admin.key");
        let data_dir = data_dir.to_owned();
        // Off the thread every scenario runs on.
        let dir = data_dir.clone();
        let start = tokio::task::spawn_blocking(move || {
            let served = Served::start(&dir, "127.0.0.1:0");
            served.ready();
            served
        });
        let served = start.await.unwrap();
        let key = std::fs::read_to_string(key_file).unwrap();
        let key = key.strip_suffix('\n').expect("a newline").to_owned();
        let client = Client::new(&served.url()).unwrap();
        let mut audit = Audit::default();
        audit.file_key(&key);
        let remote = Remote {
            client,
            served,
            data_dir,
            audit: Mutex::new(audit),
        };
        (Self::Binary(Box::new(remote)), key)
    }

    /// `close`, not a drop, of an engine: dropping joins the writer thread on the runtime.
    pub async fn close(self) {
        match self {
            Self::Engine(engine) => engine.close().await,
            Self::Binary(remote) => {
                tokio::task::spawn_blocking(move || {
                    let Remote {
                        served,
                        data_dir,
                        audit,
                        ..
                    } = *remote;
                    served.stop(&data_dir, &audit.into_inner().unwrap());
                })
                .await
                .unwrap();
            }
        }
    }
}

macro_rules! either {
    ($($name:ident($request:ty) -> $answer:ty;)*) => {
        impl Operations for Adapter {
            $(async fn $name(&self, cx: &Call, req: $request) -> Result<$answer, OpError> {
                match self {
                    Self::Engine(engine) => engine.$name(cx, req).await,
                    Self::Binary(remote) => {
                        remote.presented(cx);
                        remote.client.$name(cx, req).await.inspect_err(|error| remote.diagnostic(error))
                    }
                }
            })*

            async fn emit_batch(&self, cx: &Call, req: EmitBatchRequest) -> Result<EmitBatchResponse, OpError> {
                match self {
                    Self::Engine(engine) => engine.emit_batch(cx, req).await,
                    Self::Binary(remote) => {
                        remote.presented(cx);
                        let answer = remote.client.emit_batch(cx, req).await;
                        let audit = remote.audit.lock().unwrap();
                        match &answer {
                            Ok(batch) => audit.batch(batch),
                            Err(error) => audit.op_error(error),
                        }
                        answer
                    }
                }
            }

            async fn create_key(&self, cx: &Call, req: CreateKeyRequest) -> Result<CreatedKey, OpError> {
                match self {
                    Self::Engine(engine) => engine.create_key(cx, req).await,
                    Self::Binary(remote) => {
                        remote.presented(cx);
                        let answer = remote.client.create_key(cx, req).await;
                        match &answer {
                            Ok(created) => remote.audit.lock().unwrap().remember(created.key.expose()),
                            Err(error) => remote.diagnostic(error),
                        }
                        answer
                    }
                }
            }
        }
    };
}

either! {
    emit(EmitRequest) -> EmitResponse;
    get_event(GetEventRequest) -> Event;
    list_events(ListEventsRequest) -> EventPage;
    whoami(WhoamiRequest) -> Whoami;
    list_environments(ListEnvironmentsRequest) -> Vec<Environment>;
    list_keys(ListKeysRequest) -> KeyPage;
    revoke_key(RevokeKeyRequest) -> ();
}
