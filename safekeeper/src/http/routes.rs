use hyper::{Body, Request, Response, StatusCode, Uri};

use once_cell::sync::Lazy;
use postgres_ffi::WAL_SEGMENT_SIZE;
use safekeeper_api::models::SkTimelineInfo;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use storage_broker::proto::SafekeeperTimelineInfo;
use storage_broker::proto::TenantTimelineId as ProtoTenantTimelineId;
use tokio::task::JoinError;
use utils::http::json::display_serialize;

use crate::debug_dump;
use crate::safekeeper::ServerInfo;
use crate::safekeeper::Term;

use crate::timelines_global_map::TimelineDeleteForceResult;
use crate::GlobalTimelines;
use crate::SafeKeeperConf;
use utils::{
    auth::JwtAuth,
    http::{
        endpoint::{self, auth_middleware, check_permission_with},
        error::ApiError,
        json::{json_request, json_response},
        request::{ensure_no_body, parse_request_param},
        RequestExt, RouterBuilder,
    },
    id::{NodeId, TenantId, TenantTimelineId, TimelineId},
    lsn::Lsn,
};

use super::models::TimelineCreateRequest;

#[derive(Debug, Serialize)]
struct SafekeeperStatus {
    id: NodeId,
}

/// Healthcheck handler.
async fn status_handler(request: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permission(&request, None)?;
    let conf = get_conf(&request);
    let status = SafekeeperStatus { id: conf.my_id };
    json_response(StatusCode::OK, status)
}

fn get_conf(request: &Request<Body>) -> &SafeKeeperConf {
    request
        .data::<Arc<SafeKeeperConf>>()
        .expect("unknown state type")
        .as_ref()
}

/// Same as TermSwitchEntry, but serializes LSN using display serializer
/// in Postgres format, i.e. 0/FFFFFFFF. Used only for the API response.
#[derive(Debug, Serialize)]
struct TermSwitchApiEntry {
    pub term: Term,
    #[serde(serialize_with = "display_serialize")]
    pub lsn: Lsn,
}

/// Augment AcceptorState with epoch for convenience
#[derive(Debug, Serialize)]
struct AcceptorStateStatus {
    term: Term,
    epoch: Term,
    term_history: Vec<TermSwitchApiEntry>,
}

/// Info about timeline on safekeeper ready for reporting.
#[derive(Debug, Serialize)]
struct TimelineStatus {
    #[serde(serialize_with = "display_serialize")]
    tenant_id: TenantId,
    #[serde(serialize_with = "display_serialize")]
    timeline_id: TimelineId,
    acceptor_state: AcceptorStateStatus,
    pg_info: ServerInfo,
    #[serde(serialize_with = "display_serialize")]
    flush_lsn: Lsn,
    #[serde(serialize_with = "display_serialize")]
    timeline_start_lsn: Lsn,
    #[serde(serialize_with = "display_serialize")]
    local_start_lsn: Lsn,
    #[serde(serialize_with = "display_serialize")]
    commit_lsn: Lsn,
    #[serde(serialize_with = "display_serialize")]
    backup_lsn: Lsn,
    #[serde(serialize_with = "display_serialize")]
    peer_horizon_lsn: Lsn,
    #[serde(serialize_with = "display_serialize")]
    remote_consistent_lsn: Lsn,
}

fn check_permission(request: &Request<Body>, tenant_id: Option<TenantId>) -> Result<(), ApiError> {
    check_permission_with(request, |claims| {
        crate::auth::check_permission(claims, tenant_id)
    })
}

/// Report info about timeline.
async fn timeline_status_handler(request: Request<Body>) -> Result<Response<Body>, ApiError> {
    let ttid = TenantTimelineId::new(
        parse_request_param(&request, "tenant_id")?,
        parse_request_param(&request, "timeline_id")?,
    );
    check_permission(&request, Some(ttid.tenant_id))?;

    let tli = GlobalTimelines::get(ttid).map_err(ApiError::from)?;
    let (inmem, state) = tli.get_state();
    let flush_lsn = tli.get_flush_lsn();

    let epoch = state.acceptor_state.get_epoch(flush_lsn);
    let term_history = state
        .acceptor_state
        .term_history
        .0
        .into_iter()
        .map(|ts| TermSwitchApiEntry {
            term: ts.term,
            lsn: ts.lsn,
        })
        .collect();
    let acc_state = AcceptorStateStatus {
        term: state.acceptor_state.term,
        epoch,
        term_history,
    };

    // Note: we report in memory values which can be lost.
    let status = TimelineStatus {
        tenant_id: ttid.tenant_id,
        timeline_id: ttid.timeline_id,
        acceptor_state: acc_state,
        pg_info: state.server,
        flush_lsn,
        timeline_start_lsn: state.timeline_start_lsn,
        local_start_lsn: state.local_start_lsn,
        commit_lsn: inmem.commit_lsn,
        backup_lsn: inmem.backup_lsn,
        peer_horizon_lsn: inmem.peer_horizon_lsn,
        remote_consistent_lsn: inmem.remote_consistent_lsn,
    };
    json_response(StatusCode::OK, status)
}

async fn timeline_create_handler(mut request: Request<Body>) -> Result<Response<Body>, ApiError> {
    let request_data: TimelineCreateRequest = json_request(&mut request).await?;

    let ttid = TenantTimelineId {
        tenant_id: request_data.tenant_id,
        timeline_id: request_data.timeline_id,
    };
    check_permission(&request, Some(ttid.tenant_id))?;

    let server_info = ServerInfo {
        pg_version: request_data.pg_version,
        system_id: request_data.system_id.unwrap_or(0),
        wal_seg_size: request_data.wal_seg_size.unwrap_or(WAL_SEGMENT_SIZE as u32),
    };
    let local_start_lsn = request_data.local_start_lsn.unwrap_or_else(|| {
        request_data
            .commit_lsn
            .segment_lsn(server_info.wal_seg_size as usize)
    });
    GlobalTimelines::create(ttid, server_info, request_data.commit_lsn, local_start_lsn)
        .await
        .map_err(ApiError::InternalServerError)?;

    json_response(StatusCode::OK, ())
}

/// Deactivates the timeline and removes its data directory.
async fn timeline_delete_force_handler(
    mut request: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let ttid = TenantTimelineId::new(
        parse_request_param(&request, "tenant_id")?,
        parse_request_param(&request, "timeline_id")?,
    );
    check_permission(&request, Some(ttid.tenant_id))?;
    ensure_no_body(&mut request).await?;
    let resp = tokio::task::spawn_blocking(move || {
        // FIXME: `delete_force` can fail from both internal errors and bad requests. Add better
        // error handling here when we're able to.
        GlobalTimelines::delete_force(&ttid).map_err(ApiError::InternalServerError)
    })
    .await
    .map_err(|e: JoinError| ApiError::InternalServerError(e.into()))??;
    json_response(StatusCode::OK, resp)
}

/// Deactivates all timelines for the tenant and removes its data directory.
/// See `timeline_delete_force_handler`.
async fn tenant_delete_force_handler(
    mut request: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id = parse_request_param(&request, "tenant_id")?;
    check_permission(&request, Some(tenant_id))?;
    ensure_no_body(&mut request).await?;
    let delete_info = tokio::task::spawn_blocking(move || {
        // FIXME: `delete_force_all_for_tenant` can return an error for multiple different reasons;
        // Using an `InternalServerError` should be fixed when the types support it
        GlobalTimelines::delete_force_all_for_tenant(&tenant_id)
            .map_err(ApiError::InternalServerError)
    })
    .await
    .map_err(|e: JoinError| ApiError::InternalServerError(e.into()))??;
    json_response(
        StatusCode::OK,
        delete_info
            .iter()
            .map(|(ttid, resp)| (format!("{}", ttid.timeline_id), *resp))
            .collect::<HashMap<String, TimelineDeleteForceResult>>(),
    )
}

/// Used only in tests to hand craft required data.
async fn record_safekeeper_info(mut request: Request<Body>) -> Result<Response<Body>, ApiError> {
    let ttid = TenantTimelineId::new(
        parse_request_param(&request, "tenant_id")?,
        parse_request_param(&request, "timeline_id")?,
    );
    check_permission(&request, Some(ttid.tenant_id))?;
    let sk_info: SkTimelineInfo = json_request(&mut request).await?;
    let proto_sk_info = SafekeeperTimelineInfo {
        safekeeper_id: 0,
        tenant_timeline_id: Some(ProtoTenantTimelineId {
            tenant_id: ttid.tenant_id.as_ref().to_owned(),
            timeline_id: ttid.timeline_id.as_ref().to_owned(),
        }),
        last_log_term: sk_info.last_log_term.unwrap_or(0),
        flush_lsn: sk_info.flush_lsn.0,
        commit_lsn: sk_info.commit_lsn.0,
        remote_consistent_lsn: sk_info.remote_consistent_lsn.0,
        peer_horizon_lsn: sk_info.peer_horizon_lsn.0,
        safekeeper_connstr: sk_info.safekeeper_connstr.unwrap_or_else(|| "".to_owned()),
        backup_lsn: sk_info.backup_lsn.0,
        local_start_lsn: sk_info.local_start_lsn.0,
    };

    let tli = GlobalTimelines::get(ttid).map_err(ApiError::from)?;
    tli.record_safekeeper_info(&proto_sk_info)
        .await
        .map_err(ApiError::InternalServerError)?;

    json_response(StatusCode::OK, ())
}

fn parse_kv_str<E: fmt::Display, T: FromStr<Err = E>>(k: &str, v: &str) -> Result<T, ApiError> {
    v.parse()
        .map_err(|e| ApiError::BadRequest(anyhow::anyhow!("cannot parse {k}: {e}")))
}

/// Dump debug info about all available safekeeper state.
async fn dump_debug_handler(mut request: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permission(&request, None)?;
    ensure_no_body(&mut request).await?;

    let mut dump_all: Option<bool> = None;
    let mut dump_control_file: Option<bool> = None;
    let mut dump_memory: Option<bool> = None;
    let mut dump_disk_content: Option<bool> = None;
    let mut dump_term_history: Option<bool> = None;
    let mut tenant_id: Option<TenantId> = None;
    let mut timeline_id: Option<TimelineId> = None;

    let query = request.uri().query().unwrap_or("");
    let mut values = url::form_urlencoded::parse(query.as_bytes());

    for (k, v) in &mut values {
        match k.as_ref() {
            "dump_all" => dump_all = Some(parse_kv_str(&k, &v)?),
            "dump_control_file" => dump_control_file = Some(parse_kv_str(&k, &v)?),
            "dump_memory" => dump_memory = Some(parse_kv_str(&k, &v)?),
            "dump_disk_content" => dump_disk_content = Some(parse_kv_str(&k, &v)?),
            "dump_term_history" => dump_term_history = Some(parse_kv_str(&k, &v)?),
            "tenant_id" => tenant_id = Some(parse_kv_str(&k, &v)?),
            "timeline_id" => timeline_id = Some(parse_kv_str(&k, &v)?),
            _ => Err(ApiError::BadRequest(anyhow::anyhow!(
                "Unknown query parameter: {}",
                k
            )))?,
        }
    }

    let dump_all = dump_all.unwrap_or(false);
    let dump_control_file = dump_control_file.unwrap_or(dump_all);
    let dump_memory = dump_memory.unwrap_or(dump_all);
    let dump_disk_content = dump_disk_content.unwrap_or(dump_all);
    let dump_term_history = dump_term_history.unwrap_or(true);

    let args = debug_dump::Args {
        dump_all,
        dump_control_file,
        dump_memory,
        dump_disk_content,
        dump_term_history,
        tenant_id,
        timeline_id,
    };

    let resp = tokio::task::spawn_blocking(move || {
        debug_dump::build(args).map_err(ApiError::InternalServerError)
    })
    .await
    .map_err(|e: JoinError| ApiError::InternalServerError(e.into()))??;

    // TODO: use streaming response
    json_response(StatusCode::OK, resp)
}

/// Safekeeper http router.
pub fn make_router(conf: SafeKeeperConf) -> RouterBuilder<hyper::Body, ApiError> {
    let mut router = endpoint::make_router();
    if conf.auth.is_some() {
        router = router.middleware(auth_middleware(|request| {
            #[allow(clippy::mutable_key_type)]
            static ALLOWLIST_ROUTES: Lazy<HashSet<Uri>> =
                Lazy::new(|| ["/v1/status"].iter().map(|v| v.parse().unwrap()).collect());
            if ALLOWLIST_ROUTES.contains(request.uri()) {
                None
            } else {
                // Option<Arc<JwtAuth>> is always provided as data below, hence unwrap().
                request.data::<Option<Arc<JwtAuth>>>().unwrap().as_deref()
            }
        }))
    }

    // NB: on any changes do not forget to update the OpenAPI spec
    // located nearby (/safekeeper/src/http/openapi_spec.yaml).
    let auth = conf.auth.clone();
    router
        .data(Arc::new(conf))
        .data(auth)
        .get("/v1/status", status_handler)
        // Will be used in the future instead of implicit timeline creation
        .post("/v1/tenant/timeline", timeline_create_handler)
        .get(
            "/v1/tenant/:tenant_id/timeline/:timeline_id",
            timeline_status_handler,
        )
        .delete(
            "/v1/tenant/:tenant_id/timeline/:timeline_id",
            timeline_delete_force_handler,
        )
        .delete("/v1/tenant/:tenant_id", tenant_delete_force_handler)
        // for tests
        .post(
            "/v1/record_safekeeper_info/:tenant_id/:timeline_id",
            record_safekeeper_info,
        )
        .get("/v1/debug_dump", dump_debug_handler)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_term_switch_entry_api_serialize() {
        let state = AcceptorStateStatus {
            term: 1,
            epoch: 1,
            term_history: vec![TermSwitchApiEntry {
                term: 1,
                lsn: Lsn(0x16FFDDDD),
            }],
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(
            json,
            "{\"term\":1,\"epoch\":1,\"term_history\":[{\"term\":1,\"lsn\":\"0/16FFDDDD\"}]}"
        );
    }
}
