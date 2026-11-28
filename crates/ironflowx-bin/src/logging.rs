//! Logs each admitted call without recording the request or answer.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use ironflowx_operations::{Engine, key_id};
use ironflowx_proto::{
    BeginStepRequest, BeginStepResponse, Call, Claim, ClaimRunRequest, CompleteRunRequest,
    CompleteStepRequest, CompleteStepResponse, CreateKeyRequest, CreatedKey, EmitBatchRequest,
    EmitBatchResponse, EmitRequest, EmitResponse, Environment, Event, EventPage, Function,
    GetEventRequest, GetFunctionRequest, GetRunRequest, KeyPage, ListEnvironmentsRequest,
    ListEventsRequest, ListKeysRequest, OpError, Operation, Operations, PutFunctionRequest,
    PutFunctionResponse, RevokeKeyRequest, Run, RunEnd, StartRunRequest, StartRunResponse, Whoami,
    WhoamiRequest,
};

/// Diagnostic text may contain a path, and an operator can paste a credential into one.
pub(super) fn public_text(text: &str) -> String {
    let separator = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let mut safe = String::with_capacity(text.len());
    for part in text.split_inclusive(separator) {
        let word = part.trim_end_matches(separator);
        if word.contains("ixk_") || word.len() >= 43 {
            safe.push_str("<hidden>");
        } else {
            safe.push_str(word);
        }
        for c in part[word.len()..].chars() {
            if c.is_control() {
                safe.extend(c.escape_default());
            } else {
                safe.push(c);
            }
        }
    }
    safe
}

pub struct Logged(pub Arc<Engine>);

struct CallLog {
    operation: Operation,
    id: Option<ironflowx_core::KeyId>,
    outcome: &'static str,
    started: Instant,
}

impl Drop for CallLog {
    fn drop(&mut self) {
        tracing::info!(
            operation = self.operation.rpc(),
            key_id = self.id.map(tracing::field::display),
            outcome = self.outcome,
            duration_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            "call"
        );
    }
}

async fn record<T>(
    op: Operation,
    cx: &Call,
    answer: impl Future<Output = Result<T, OpError>>,
) -> Result<T, OpError> {
    let mut log = CallLog {
        operation: op,
        id: cx.key().and_then(key_id),
        outcome: "cancelled",
        started: Instant::now(),
    };
    let answer = answer.await;
    log.outcome = answer.as_ref().err().map_or("success", |e| e.kind.as_str());
    answer
}

macro_rules! logged {
    ($method:ident, $operation:ident, $request:ty, $answer:ty) => {
        async fn $method(&self, cx: &Call, req: $request) -> Result<$answer, OpError> {
            record(Operation::$operation, cx, self.0.$method(cx, req)).await
        }
    };
}

impl Operations for Logged {
    logged!(emit, Emit, EmitRequest, EmitResponse);
    logged!(emit_batch, EmitBatch, EmitBatchRequest, EmitBatchResponse);
    logged!(get_event, GetEvent, GetEventRequest, Event);
    logged!(list_events, ListEvents, ListEventsRequest, EventPage);
    async fn whoami(&self, cx: &Call, req: WhoamiRequest) -> Result<Whoami, OpError> {
        #[cfg(feature = "__fault-points")]
        assert!(
            !PANIC_IN_HANDLER.load(std::sync::atomic::Ordering::Relaxed),
            "fixture panic in whoami handler"
        );
        record(Operation::Whoami, cx, self.0.whoami(cx, req)).await
    }
    logged!(
        list_environments,
        ListEnvironments,
        ListEnvironmentsRequest,
        Vec<Environment>
    );
    logged!(create_key, CreateKey, CreateKeyRequest, CreatedKey);
    logged!(list_keys, ListKeys, ListKeysRequest, KeyPage);
    logged!(revoke_key, RevokeKey, RevokeKeyRequest, ());
    logged!(
        put_function,
        PutFunction,
        PutFunctionRequest,
        PutFunctionResponse
    );
    logged!(get_function, GetFunction, GetFunctionRequest, Function);
    logged!(start_run, StartRun, StartRunRequest, StartRunResponse);
    logged!(get_run, GetRun, GetRunRequest, Run);
    logged!(claim_run, ClaimRun, ClaimRunRequest, Option<Claim>);
    logged!(begin_step, BeginStep, BeginStepRequest, BeginStepResponse);
    logged!(
        complete_step,
        CompleteStep,
        CompleteStepRequest,
        CompleteStepResponse
    );
    logged!(complete_run, CompleteRun, CompleteRunRequest, RunEnd);
}

#[cfg(feature = "__fault-points")]
static PANIC_IN_HANDLER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Arms the fixture's panic point in the host's Whoami handler.
#[cfg(feature = "__fault-points")]
pub fn panic_in_handler() {
    PANIC_IN_HANDLER.store(true, std::sync::atomic::Ordering::Relaxed);
}
