/*
 * Parseable Server (C) 2022 - 2024 Parseable, Inc.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 */

pub mod utils;
use futures::{StreamExt, future, stream};
use lazy_static::lazy_static;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

use actix_web::Responder;
use actix_web::http::header::{self, HeaderMap};
use actix_web::web::Path;
use bytes::Bytes;
use chrono::Utc;
use clokwerk::{AsyncScheduler, Interval};
use http::{StatusCode, header as http_header};
use itertools::Itertools;
use serde::de::{DeserializeOwned, Error};
use serde_json::error::Error as SerdeError;
use serde_json::{Value as JsonValue, to_vec};
use tracing::{error, info, warn};
use url::Url;
use utils::{IngestionStats, QueriedStats, StorageStats, check_liveness, to_url_string};

use crate::INTRA_CLUSTER_CLIENT;
use crate::handlers::http::ingest::ingest_internal_stream;
use crate::handlers::http::query::{Query, QueryError, TIME_ELAPSED_HEADER};
use crate::metrics::prom_utils::Metrics;
use crate::option::Mode;
use crate::parseable::PARSEABLE;
use crate::rbac::role::model::DefaultPrivilege;
use crate::rbac::user::User;
use crate::stats::Stats;
use crate::storage::{ObjectStorageError, ObjectStoreFormat};

use super::base_path_without_preceding_slash;
use super::ingest::PostError;
use super::logstream::error::StreamError;
use super::modal::{IngestorMetadata, Metadata, NodeMetadata, NodeType, QuerierMetadata};
use super::rbac::RBACError;
use super::role::RoleError;

pub const PMETA_STREAM_NAME: &str = "pmeta";
pub const BILLING_METRICS_STREAM_NAME: &str = "pbilling";

const CLUSTER_METRICS_INTERVAL_SECONDS: Interval = clokwerk::Interval::Minutes(1);

lazy_static! {
    static ref QUERIER_MAP: Arc<RwLock<HashMap<String, QuerierStatus>>> =
        Arc::new(RwLock::new(HashMap::new()));
    static ref LAST_USED_QUERIER: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
}

#[derive(Debug, serde::Serialize, Clone)]
pub struct BillingMetricEvent {
    pub node_address: String,
    pub node_type: String,
    pub metric_type: String,
    pub date: String,
    pub value: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub event_type: String,
    pub event_time: chrono::NaiveDateTime,
}

// Internal structure for collecting metrics from prometheus
#[derive(Debug, Default)]
struct BillingMetricsCollector {
    pub node_address: String,
    pub node_type: String,
    pub total_events_ingested_by_date: HashMap<String, u64>,
    pub total_events_ingested_size_by_date: HashMap<String, u64>,
    pub total_parquets_stored_by_date: HashMap<String, u64>,
    pub total_parquets_stored_size_by_date: HashMap<String, u64>,
    pub total_query_calls_by_date: HashMap<String, u64>,
    pub total_files_scanned_in_query_by_date: HashMap<String, u64>,
    pub total_bytes_scanned_in_query_by_date: HashMap<String, u64>,
    pub total_object_store_calls_by_date: HashMap<String, HashMap<String, u64>>, // method -> date -> count
    pub total_files_scanned_in_object_store_calls_by_date: HashMap<String, HashMap<String, u64>>,
    pub total_bytes_scanned_in_object_store_calls_by_date: HashMap<String, HashMap<String, u64>>,
    pub total_input_llm_tokens_by_date: HashMap<String, HashMap<String, HashMap<String, u64>>>, // provider -> model -> date -> count
    pub total_output_llm_tokens_by_date: HashMap<String, HashMap<String, HashMap<String, u64>>>,
    pub event_time: chrono::NaiveDateTime,
}

impl BillingMetricsCollector {
    pub fn new(node_address: String, node_type: String) -> Self {
        Self {
            node_address,
            node_type,
            event_time: Utc::now().naive_utc(),
            ..Default::default()
        }
    }

    /// Convert the collector into individual billing metric events, excluding empty collections
    pub fn into_events(self) -> Vec<BillingMetricEvent> {
        let mut events = Vec::new();

        // Add all different types of metrics
        self.add_simple_metrics(&mut events);
        self.add_object_store_metrics(&mut events);
        self.add_llm_metrics(&mut events);

        events
    }

    /// Add simple date-based metrics to the events vector
    fn add_simple_metrics(&self, events: &mut Vec<BillingMetricEvent>) {
        let add_simple_metric = |events: &mut Vec<BillingMetricEvent>,
                                 metric_type: &str,
                                 data: &HashMap<String, u64>| {
            for (date, value) in data {
                if *value > 0 {
                    events.push(BillingMetricEvent {
                        node_address: self.node_address.clone(),
                        node_type: self.node_type.clone(),
                        metric_type: metric_type.to_string(),
                        date: date.clone(),
                        value: *value,
                        method: None,
                        provider: None,
                        model: None,
                        event_type: "billing-metrics".to_string(),
                        event_time: self.event_time,
                    });
                }
            }
        };

        // Add simple metrics (only if not empty)
        if !self.total_events_ingested_by_date.is_empty() {
            add_simple_metric(
                events,
                "total_events_ingested",
                &self.total_events_ingested_by_date,
            );
        }
        if !self.total_events_ingested_size_by_date.is_empty() {
            add_simple_metric(
                events,
                "total_events_ingested_size",
                &self.total_events_ingested_size_by_date,
            );
        }
        if !self.total_parquets_stored_by_date.is_empty() {
            add_simple_metric(
                events,
                "total_parquets_stored",
                &self.total_parquets_stored_by_date,
            );
        }
        if !self.total_parquets_stored_size_by_date.is_empty() {
            add_simple_metric(
                events,
                "total_parquets_stored_size",
                &self.total_parquets_stored_size_by_date,
            );
        }
        if !self.total_query_calls_by_date.is_empty() {
            add_simple_metric(events, "total_query_calls", &self.total_query_calls_by_date);
        }
        if !self.total_files_scanned_in_query_by_date.is_empty() {
            add_simple_metric(
                events,
                "total_files_scanned_in_query",
                &self.total_files_scanned_in_query_by_date,
            );
        }
        if !self.total_bytes_scanned_in_query_by_date.is_empty() {
            add_simple_metric(
                events,
                "total_bytes_scanned_in_query",
                &self.total_bytes_scanned_in_query_by_date,
            );
        }
    }

    /// Add object store metrics (method-based) to the events vector
    fn add_object_store_metrics(&self, events: &mut Vec<BillingMetricEvent>) {
        let object_store_metrics = [
            (
                "total_object_store_calls",
                &self.total_object_store_calls_by_date,
            ),
            (
                "total_files_scanned_in_object_store_calls",
                &self.total_files_scanned_in_object_store_calls_by_date,
            ),
            (
                "total_bytes_scanned_in_object_store_calls",
                &self.total_bytes_scanned_in_object_store_calls_by_date,
            ),
        ];

        for (metric_type, data) in object_store_metrics {
            if !data.is_empty() {
                for (method, dates) in data {
                    for (date, value) in dates {
                        if *value > 0 {
                            events.push(BillingMetricEvent {
                                node_address: self.node_address.clone(),
                                node_type: self.node_type.clone(),
                                metric_type: metric_type.to_string(),
                                date: date.clone(),
                                value: *value,
                                method: Some(method.clone()),
                                provider: None,
                                model: None,
                                event_type: "billing-metrics".to_string(),
                                event_time: self.event_time,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Add LLM metrics (provider/model-based) to the events vector
    fn add_llm_metrics(&self, events: &mut Vec<BillingMetricEvent>) {
        let llm_metrics = [
            (
                "total_input_llm_tokens",
                &self.total_input_llm_tokens_by_date,
            ),
            (
                "total_output_llm_tokens",
                &self.total_output_llm_tokens_by_date,
            ),
        ];

        for (metric_type, data) in llm_metrics {
            if !data.is_empty() {
                for (provider, models) in data {
                    for (model, dates) in models {
                        for (date, value) in dates {
                            if *value > 0 {
                                events.push(BillingMetricEvent {
                                    node_address: self.node_address.clone(),
                                    node_type: self.node_type.clone(),
                                    metric_type: metric_type.to_string(),
                                    date: date.clone(),
                                    value: *value,
                                    method: None,
                                    provider: Some(provider.clone()),
                                    model: Some(model.clone()),
                                    event_type: "billing-metrics".to_string(),
                                    event_time: self.event_time,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn for_each_live_ingestor<F, Fut, E>(api_fn: F) -> Result<(), E>
where
    F: Fn(NodeMetadata) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(), E>> + Send,
    E: From<anyhow::Error> + Send + Sync + 'static,
{
    let ingestor_infos: Vec<NodeMetadata> =
        get_node_info(NodeType::Ingestor).await.map_err(|err| {
            error!("Fatal: failed to get ingestor info: {:?}", err);
            E::from(err)
        })?;

    let mut live_ingestors = Vec::new();
    for ingestor in ingestor_infos {
        if utils::check_liveness(&ingestor.domain_name).await {
            live_ingestors.push(ingestor);
        } else {
            warn!("Ingestor {} is not live", ingestor.domain_name);
        }
    }

    // Process all live ingestors in parallel
    let results = futures::future::join_all(live_ingestors.into_iter().map(|ingestor| {
        let api_fn = api_fn.clone();
        async move { api_fn(ingestor).await }
    }))
    .await;

    // collect results
    for result in results {
        result?;
    }

    Ok(())
}

// forward the create/update stream request to all ingestors to keep them in sync
pub async fn sync_streams_with_ingestors(
    headers: HeaderMap,
    body: Bytes,
    stream_name: &str,
) -> Result<(), StreamError> {
    let mut reqwest_headers = http_header::HeaderMap::new();

    for (key, value) in headers.iter() {
        reqwest_headers.insert(key.clone(), value.clone());
    }

    let body_clone = body.clone();
    let stream_name = stream_name.to_string();
    let reqwest_headers_clone = reqwest_headers.clone();

    for_each_live_ingestor(
        move |ingestor| {
            let url = format!(
                "{}{}/logstream/{}/sync",
                ingestor.domain_name,
                base_path_without_preceding_slash(),
                stream_name
            );
            let headers = reqwest_headers_clone.clone();
            let body = body_clone.clone();
            async move {
                let res = INTRA_CLUSTER_CLIENT
                    .put(url)
                    .headers(headers)
                    .header(header::AUTHORIZATION, &ingestor.token)
                    .body(body)
                    .send()
                    .await
                    .map_err(|err| {
                        error!(
                            "Fatal: failed to forward upsert stream request to ingestor: {}\n Error: {:?}",
                            ingestor.domain_name, err
                        );
                        StreamError::Network(err)
                    })?;

                if !res.status().is_success() {
                    error!(
                        "failed to forward upsert stream request to ingestor: {}\nResponse Returned: {:?}",
                        ingestor.domain_name,
                        res.text().await
                    );
                }
                Ok(())
            }
        }
    ).await
}

// forward the demo data request to one of the live ingestor
pub async fn get_demo_data_from_ingestor(action: &str) -> Result<(), PostError> {
    let ingestor_infos: Vec<NodeMetadata> =
        get_node_info(NodeType::Ingestor).await.map_err(|err| {
            error!("Fatal: failed to get ingestor info: {:?}", err);
            PostError::Invalid(err)
        })?;

    let mut live_ingestors: Vec<NodeMetadata> = Vec::new();
    for ingestor in ingestor_infos {
        if utils::check_liveness(&ingestor.domain_name).await {
            live_ingestors.push(ingestor);
            break;
        }
    }

    if live_ingestors.is_empty() {
        return Err(PostError::Invalid(anyhow::anyhow!(
            "No live ingestors found"
        )));
    }

    // Pick the first live ingestor
    let ingestor = &live_ingestors[0];

    let url = format!(
        "{}{}/demodata?action={action}",
        ingestor.domain_name,
        base_path_without_preceding_slash()
    );

    let res = INTRA_CLUSTER_CLIENT
        .get(url)
        .header(header::AUTHORIZATION, &ingestor.token)
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .map_err(|err| {
            error!(
                "Fatal: failed to forward request to ingestor: {}\n Error: {:?}",
                ingestor.domain_name, err
            );
            PostError::Invalid(err.into())
        })?;

    if !res.status().is_success() {
        return Err(PostError::Invalid(anyhow::anyhow!(
            "failed to forward request to ingestor: {}\nResponse status: {}",
            ingestor.domain_name,
            res.status()
        )));
    }

    Ok(())
}

// forward the role update request to all ingestors to keep them in sync
pub async fn sync_users_with_roles_with_ingestors(
    userid: &str,
    role: &HashSet<String>,
    operation: &str,
) -> Result<(), RBACError> {
    match operation {
        "add" | "remove" => {}
        _ => return Err(RBACError::InvalidSyncOperation(operation.to_string())),
    }

    let role_data = to_vec(&role.clone()).map_err(|err| {
        error!("Fatal: failed to serialize role: {:?}", err);
        RBACError::SerdeError(err)
    })?;

    let userid = userid.to_owned();

    let op = operation.to_string();

    for_each_live_ingestor(move |ingestor| {
        let url = format!(
            "{}{}/user/{}/role/sync/{}",
            ingestor.domain_name,
            base_path_without_preceding_slash(),
            userid,
            op
        );

        let role_data = role_data.clone();

        async move {
            let res = INTRA_CLUSTER_CLIENT
                .patch(url)
                .header(header::AUTHORIZATION, &ingestor.token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(role_data)
                .send()
                .await
                .map_err(|err| {
                    error!(
                        "Fatal: failed to forward request to ingestor: {}\n Error: {:?}",
                        ingestor.domain_name, err
                    );
                    RBACError::Network(err)
                })?;

            if !res.status().is_success() {
                error!(
                    "failed to forward request to ingestor: {}\nResponse Returned: {:?}",
                    ingestor.domain_name,
                    res.text().await
                );
            }

            Ok(())
        }
    })
    .await
}

// forward the delete user request to all ingestors to keep them in sync
pub async fn sync_user_deletion_with_ingestors(userid: &str) -> Result<(), RBACError> {
    let userid = userid.to_owned();

    for_each_live_ingestor(move |ingestor| {
        let url = format!(
            "{}{}/user/{}/sync",
            ingestor.domain_name,
            base_path_without_preceding_slash(),
            userid
        );

        async move {
            let res = INTRA_CLUSTER_CLIENT
                .delete(url)
                .header(header::AUTHORIZATION, &ingestor.token)
                .send()
                .await
                .map_err(|err| {
                    error!(
                        "Fatal: failed to forward request to ingestor: {}\n Error: {:?}",
                        ingestor.domain_name, err
                    );
                    RBACError::Network(err)
                })?;

            if !res.status().is_success() {
                error!(
                    "failed to forward request to ingestor: {}\nResponse Returned: {:?}",
                    ingestor.domain_name,
                    res.text().await
                );
            }

            Ok(())
        }
    })
    .await
}

// forward the create user request to all ingestors to keep them in sync
pub async fn sync_user_creation_with_ingestors(
    user: User,
    role: &Option<HashSet<String>>,
) -> Result<(), RBACError> {
    let mut user = user.clone();

    if let Some(role) = role {
        user.roles.clone_from(role);
    }
    let userid = user.userid();

    let user_data = to_vec(&user).map_err(|err| {
        error!("Fatal: failed to serialize user: {:?}", err);
        RBACError::SerdeError(err)
    })?;

    let userid = userid.to_string();

    for_each_live_ingestor(move |ingestor| {
        let url = format!(
            "{}{}/user/{}/sync",
            ingestor.domain_name,
            base_path_without_preceding_slash(),
            userid
        );

        let user_data = user_data.clone();

        async move {
            let res = INTRA_CLUSTER_CLIENT
                .post(url)
                .header(header::AUTHORIZATION, &ingestor.token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(user_data)
                .send()
                .await
                .map_err(|err| {
                    error!(
                        "Fatal: failed to forward request to ingestor: {}\n Error: {:?}",
                        ingestor.domain_name, err
                    );
                    RBACError::Network(err)
                })?;

            if !res.status().is_success() {
                error!(
                    "failed to forward request to ingestor: {}\nResponse Returned: {:?}",
                    ingestor.domain_name,
                    res.text().await
                );
            }

            Ok(())
        }
    })
    .await
}

// forward the password reset request to all ingestors to keep them in sync
pub async fn sync_password_reset_with_ingestors(username: &str) -> Result<(), RBACError> {
    let username = username.to_owned();

    for_each_live_ingestor(move |ingestor| {
        let url = format!(
            "{}{}/user/{}/generate-new-password/sync",
            ingestor.domain_name,
            base_path_without_preceding_slash(),
            username
        );

        async move {
            let res = INTRA_CLUSTER_CLIENT
                .post(url)
                .header(header::AUTHORIZATION, &ingestor.token)
                .header(header::CONTENT_TYPE, "application/json")
                .send()
                .await
                .map_err(|err| {
                    error!(
                        "Fatal: failed to forward request to ingestor: {}\n Error: {:?}",
                        ingestor.domain_name, err
                    );
                    RBACError::Network(err)
                })?;

            if !res.status().is_success() {
                error!(
                    "failed to forward request to ingestor: {}\nResponse Returned: {:?}",
                    ingestor.domain_name,
                    res.text().await
                );
            }

            Ok(())
        }
    })
    .await
}

// forward the put role request to all ingestors to keep them in sync
pub async fn sync_role_update_with_ingestors(
    name: String,
    privileges: Vec<DefaultPrivilege>,
) -> Result<(), RoleError> {
    for_each_live_ingestor(move |ingestor| {
        let url = format!(
            "{}{}/role/{}/sync",
            ingestor.domain_name,
            base_path_without_preceding_slash(),
            name
        );

        let privileges = privileges.clone();

        async move {
            let res = INTRA_CLUSTER_CLIENT
                .put(url)
                .header(header::AUTHORIZATION, &ingestor.token)
                .header(header::CONTENT_TYPE, "application/json")
                .json(&privileges)
                .send()
                .await
                .map_err(|err| {
                    error!(
                        "Fatal: failed to forward request to ingestor: {}\n Error: {:?}",
                        ingestor.domain_name, err
                    );
                    RoleError::Network(err)
                })?;

            if !res.status().is_success() {
                error!(
                    "failed to forward request to ingestor: {}\nResponse Returned: {:?}",
                    ingestor.domain_name,
                    res.text().await
                );
            }

            Ok(())
        }
    })
    .await
}

pub fn fetch_daily_stats(
    date: &str,
    stream_meta_list: &[ObjectStoreFormat],
) -> Result<Stats, StreamError> {
    // for the given date, get the stats from the ingestors
    let mut events_ingested = 0;
    let mut ingestion_size = 0;
    let mut storage_size = 0;

    for meta in stream_meta_list.iter() {
        for manifest in meta.snapshot.manifest_list.iter() {
            if manifest.time_lower_bound.date_naive().to_string() == date {
                events_ingested += manifest.events_ingested;
                ingestion_size += manifest.ingestion_size;
                storage_size += manifest.storage_size;
            }
        }
    }

    let stats = Stats {
        events: events_ingested,
        ingestion: ingestion_size,
        storage: storage_size,
    };
    Ok(stats)
}

/// get the cumulative stats from all ingestors
pub async fn fetch_stats_from_ingestors(
    stream_name: &str,
) -> Result<Vec<utils::QueriedStats>, StreamError> {
    let obs = PARSEABLE
        .metastore
        .get_all_stream_jsons(stream_name, Some(Mode::Ingest))
        .await?;

    let mut ingestion_size = 0u64;
    let mut storage_size = 0u64;
    let mut count = 0u64;
    let mut lifetime_ingestion_size = 0u64;
    let mut lifetime_storage_size = 0u64;
    let mut lifetime_count = 0u64;
    let mut deleted_ingestion_size = 0u64;
    let mut deleted_storage_size = 0u64;
    let mut deleted_count = 0u64;
    for ob in obs {
        let stream_metadata: ObjectStoreFormat =
            serde_json::from_slice(&ob).expect("stream.json is valid json");

        count += stream_metadata.stats.current_stats.events;
        ingestion_size += stream_metadata.stats.current_stats.ingestion;
        storage_size += stream_metadata.stats.current_stats.storage;
        lifetime_count += stream_metadata.stats.lifetime_stats.events;
        lifetime_ingestion_size += stream_metadata.stats.lifetime_stats.ingestion;
        lifetime_storage_size += stream_metadata.stats.lifetime_stats.storage;
        deleted_count += stream_metadata.stats.deleted_stats.events;
        deleted_ingestion_size += stream_metadata.stats.deleted_stats.ingestion;
        deleted_storage_size += stream_metadata.stats.deleted_stats.storage;
    }

    let qs = QueriedStats::new(
        "",
        Utc::now(),
        IngestionStats::new(
            count,
            ingestion_size,
            lifetime_count,
            lifetime_ingestion_size,
            deleted_count,
            deleted_ingestion_size,
            "json",
        ),
        StorageStats::new(
            storage_size,
            lifetime_storage_size,
            deleted_storage_size,
            "parquet",
        ),
    );

    Ok(vec![qs])
}

/// send a delete stream request to all ingestors
pub async fn send_stream_delete_request(
    url: &str,
    ingestor: IngestorMetadata,
) -> Result<(), StreamError> {
    if !utils::check_liveness(&ingestor.domain_name).await {
        return Ok(());
    }
    let resp = INTRA_CLUSTER_CLIENT
        .delete(url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, ingestor.token)
        .send()
        .await
        .map_err(|err| {
            // log the error and return a custom error
            error!(
                "Fatal: failed to delete stream: {}\n Error: {:?}",
                ingestor.domain_name, err
            );
            StreamError::Network(err)
        })?;

    // if the response is not successful, log the error and return a custom error
    // this could be a bit too much, but we need to be sure it covers all cases
    if !resp.status().is_success() {
        error!(
            "failed to delete stream: {}\nResponse Returned: {:?}",
            ingestor.domain_name,
            resp.text().await
        );
    }

    Ok(())
}

/// send a retention cleanup request to all ingestors
pub async fn send_retention_cleanup_request(
    url: &str,
    ingestor: IngestorMetadata,
    dates: &[String],
) -> Result<(), ObjectStorageError> {
    let resp = INTRA_CLUSTER_CLIENT
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, ingestor.token)
        .json(dates)
        .send()
        .await
        .map_err(|err| {
            // log the error and return a custom error
            error!(
                "Fatal: failed to perform cleanup on retention: {}\n Error: {:?}",
                ingestor.domain_name, err
            );
            ObjectStorageError::Custom(err.to_string())
        })?;

    // if the response is not successful, log the error and return a custom error
    // this could be a bit too much, but we need to be sure it covers all cases
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        error!(
            "failed to perform cleanup on retention: {}\nResponse Returned: {:?}",
            ingestor.domain_name, body
        );
        return Err(ObjectStorageError::Custom(format!(
            "failed to perform cleanup on retention: {}\nResponse Returned: {:?}",
            ingestor.domain_name, body
        )));
    }

    Ok(())
}

/// Fetches cluster information for all nodes (ingestor, indexer, querier and prism)
pub async fn get_cluster_info() -> Result<impl Responder, StreamError> {
    // Get querier, ingestor and indexer metadata concurrently
    let (prism_result, querier_result, ingestor_result, indexer_result) = future::join4(
        get_node_info(NodeType::Prism),
        get_node_info(NodeType::Querier),
        get_node_info(NodeType::Ingestor),
        get_node_info(NodeType::Indexer),
    )
    .await;

    // Handle prism metadata result
    let prism_metadata: Vec<NodeMetadata> = prism_result
        .map_err(|err| {
            error!("Fatal: failed to get prism info: {:?}", err);
            PostError::Invalid(err)
        })
        .map_err(|err| StreamError::Anyhow(err.into()))?;

    // Handle querier metadata result
    let querier_metadata: Vec<NodeMetadata> = querier_result
        .map_err(|err| {
            error!("Fatal: failed to get querier info: {:?}", err);
            PostError::Invalid(err)
        })
        .map_err(|err| StreamError::Anyhow(err.into()))?;

    // Handle ingestor metadata result
    let ingestor_metadata: Vec<NodeMetadata> = ingestor_result
        .map_err(|err| {
            error!("Fatal: failed to get ingestor info: {:?}", err);
            PostError::Invalid(err)
        })
        .map_err(|err| StreamError::Anyhow(err.into()))?;

    // Handle indexer metadata result
    let indexer_metadata: Vec<NodeMetadata> = indexer_result
        .map_err(|err| {
            error!("Fatal: failed to get indexer info: {:?}", err);
            PostError::Invalid(err)
        })
        .map_err(|err| StreamError::Anyhow(err.into()))?;

    // Fetch info for all nodes concurrently
    let (prism_infos, querier_infos, ingestor_infos, indexer_infos) = future::join4(
        fetch_nodes_info(prism_metadata),
        fetch_nodes_info(querier_metadata),
        fetch_nodes_info(ingestor_metadata),
        fetch_nodes_info(indexer_metadata),
    )
    .await;

    // Combine results from all node types
    let mut infos = Vec::new();
    infos.extend(prism_infos?);
    infos.extend(querier_infos?);
    infos.extend(ingestor_infos?);
    infos.extend(indexer_infos?);
    Ok(actix_web::HttpResponse::Ok().json(infos))
}

/// Fetches info for a single node
/// call the about endpoint of the node
/// construct the ClusterInfo struct and return it
async fn fetch_node_info<T: Metadata>(node: &T) -> Result<utils::ClusterInfo, StreamError> {
    let uri = Url::parse(&format!(
        "{}{}/about",
        node.domain_name(),
        base_path_without_preceding_slash()
    ))
    .expect("should always be a valid url");

    let resp = INTRA_CLUSTER_CLIENT
        .get(uri)
        .header(header::AUTHORIZATION, node.token().to_owned())
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await;

    let (reachable, staging_path, error, status) = if let Ok(resp) = resp {
        let status = Some(resp.status().to_string());

        let resp_data = resp.bytes().await.map_err(|err| {
            error!("Fatal: failed to parse node info to bytes: {:?}", err);
            StreamError::Network(err)
        })?;

        let sp = serde_json::from_slice::<JsonValue>(&resp_data)
            .map_err(|err| {
                error!("Fatal: failed to parse node info: {:?}", err);
                StreamError::SerdeError(err)
            })?
            .get("staging")
            .ok_or(StreamError::SerdeError(SerdeError::missing_field(
                "staging",
            )))?
            .as_str()
            .ok_or(StreamError::SerdeError(SerdeError::custom(
                "staging path not a string/ not provided",
            )))?
            .to_string();

        (true, sp, None, status)
    } else {
        (
            false,
            "".to_owned(),
            resp.as_ref().err().map(|e| e.to_string()),
            resp.unwrap_err().status().map(|s| s.to_string()),
        )
    };

    Ok(utils::ClusterInfo::new(
        node.domain_name(),
        reachable,
        staging_path,
        PARSEABLE.storage.get_endpoint(),
        error,
        status,
        node.node_type(),
    ))
}

/// Fetches info for multiple nodes in parallel
async fn fetch_nodes_info<T: Metadata>(
    nodes: Vec<T>,
) -> Result<Vec<utils::ClusterInfo>, StreamError> {
    let nodes_len = nodes.len();
    if nodes_len == 0 {
        return Ok(vec![]);
    }
    let results = stream::iter(nodes)
        .map(|node| async move { fetch_node_info(&node).await })
        .buffer_unordered(nodes_len) // No concurrency limit
        .collect::<Vec<_>>()
        .await;

    // Collect results, propagating any errors
    let mut infos = Vec::with_capacity(results.len());
    for result in results {
        infos.push(result?);
    }

    Ok(infos)
}

pub async fn get_cluster_metrics() -> Result<impl Responder, PostError> {
    let dresses = fetch_cluster_metrics().await.map_err(|err| {
        error!("Fatal: failed to fetch cluster metrics: {:?}", err);
        PostError::Invalid(err.into())
    })?;

    Ok(actix_web::HttpResponse::Ok().json(dresses))
}

/// get node info for a specific node type
/// this is used to get the node info for ingestor, indexer, querier and prism
/// it will return the metadata for all nodes of that type
pub async fn get_node_info<T: Metadata + DeserializeOwned>(
    node_type: NodeType,
) -> anyhow::Result<Vec<T>> {
    let metadata = PARSEABLE
        .metastore
        .get_node_metadata(node_type)
        .await?
        .iter()
        .filter_map(|x| match serde_json::from_slice::<T>(x) {
            Ok(val) => Some(val),
            Err(e) => {
                error!("Failed to parse node metadata: {:?}", e);
                None
            }
        })
        .collect();

    Ok(metadata)
}
/// remove a node from the cluster
/// check liveness of the node
/// if the node is live, return an error
/// if the node is not live, remove the node from the cluster
/// remove the node metadata from the object store
pub async fn remove_node(node_url: Path<String>) -> Result<impl Responder, PostError> {
    let domain_name = to_url_string(node_url.into_inner());

    if check_liveness(&domain_name).await {
        return Err(PostError::Invalid(anyhow::anyhow!(
            "The node is currently live and cannot be removed"
        )));
    }

    // Delete ingestor metadata
    let removed_ingestor = PARSEABLE
        .metastore
        .delete_node_metadata(&domain_name, NodeType::Ingestor)
        .await?;

    // Delete indexer metadata
    let removed_indexer = PARSEABLE
        .metastore
        .delete_node_metadata(&domain_name, NodeType::Indexer)
        .await?;

    // Delete querier metadata
    let removed_querier = PARSEABLE
        .metastore
        .delete_node_metadata(&domain_name, NodeType::Querier)
        .await?;

    // Delete prism metadata
    let removed_prism = PARSEABLE
        .metastore
        .delete_node_metadata(&domain_name, NodeType::Prism)
        .await?;

    if removed_ingestor || removed_indexer || removed_querier || removed_prism {
        return Ok((
            format!("node {domain_name} removed successfully"),
            StatusCode::OK,
        ));
    }
    Err(PostError::Invalid(anyhow::anyhow!(
        "node {domain_name} not found"
    )))
}

/// Fetches metrics for a single node
/// This function is used to fetch metrics from a single node
/// It checks if the node is live and then fetches the metrics
/// If the node is not live, it returns None
async fn fetch_node_metrics<T>(node: &T) -> Result<Option<Metrics>, PostError>
where
    T: Metadata + Send + Sync + 'static,
{
    // Format the metrics URL
    let uri = Url::parse(&format!(
        "{}{}/metrics",
        node.domain_name(),
        base_path_without_preceding_slash()
    ))
    .map_err(|err| PostError::Invalid(anyhow::anyhow!("Invalid URL in node metadata: {}", err)))?;

    // Check if the node is live
    if !check_liveness(node.domain_name()).await {
        warn!("node {} is not live", node.domain_name());
        return Ok(None);
    }

    // Fetch metrics
    let res = INTRA_CLUSTER_CLIENT
        .get(uri)
        .header(header::AUTHORIZATION, node.token())
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await;

    match res {
        Ok(res) => {
            let text = res.text().await.map_err(PostError::NetworkError)?;
            let lines: Vec<Result<String, std::io::Error>> =
                text.lines().map(|line| Ok(line.to_owned())).collect_vec();

            let sample = prometheus_parse::Scrape::parse(lines.into_iter())
                .map_err(|err| PostError::CustomError(err.to_string()))?
                .samples;

            let metrics = Metrics::from_prometheus_samples(sample, node)
                .await
                .map_err(|err| {
                    error!("Fatal: failed to get node metrics: {:?}", err);
                    PostError::Invalid(err.into())
                })?;

            Ok(Some(metrics))
        }
        Err(_) => {
            warn!(
                "Failed to fetch metrics from node: {}\n",
                node.domain_name()
            );
            Ok(None)
        }
    }
}

/// Fetches metrics from multiple nodes in parallel
async fn fetch_nodes_metrics<T>(nodes: Vec<T>) -> Result<Vec<Metrics>, PostError>
where
    T: Metadata + Send + Sync + 'static,
{
    let nodes_len = nodes.len();
    if nodes_len == 0 {
        return Ok(vec![]);
    }
    let results = stream::iter(nodes)
        .map(|node| async move { fetch_node_metrics(&node).await })
        .buffer_unordered(nodes_len) // No concurrency limit
        .collect::<Vec<_>>()
        .await;

    // Process results
    let mut metrics = Vec::new();
    for result in results {
        match result {
            Ok(Some(node_metrics)) => metrics.push(node_metrics),
            Ok(_) => {} // node was not live or metrics couldn't be fetched
            Err(err) => return Err(err),
        }
    }

    Ok(metrics)
}

/// Main function to fetch cluster metrics
/// fetches node info for all nodes
/// fetches metrics for all nodes
/// combines all metrics into a single vector
async fn fetch_cluster_metrics() -> Result<Vec<Metrics>, PostError> {
    // Get ingestor and indexer metadata concurrently
    let (prism_result, querier_result, ingestor_result, indexer_result) = future::join4(
        get_node_info(NodeType::Prism),
        get_node_info(NodeType::Querier),
        get_node_info(NodeType::Ingestor),
        get_node_info(NodeType::Indexer),
    )
    .await;

    // Handle prism metadata result
    let prism_metadata: Vec<NodeMetadata> = prism_result.map_err(|err| {
        error!("Fatal: failed to get prism info: {:?}", err);
        PostError::Invalid(err)
    })?;

    // Handle querier metadata result
    let querier_metadata: Vec<NodeMetadata> = querier_result.map_err(|err| {
        error!("Fatal: failed to get querier info: {:?}", err);
        PostError::Invalid(err)
    })?;
    // Handle ingestor metadata result
    let ingestor_metadata: Vec<NodeMetadata> = ingestor_result.map_err(|err| {
        error!("Fatal: failed to get ingestor info: {:?}", err);
        PostError::Invalid(err)
    })?;
    // Handle indexer metadata result
    let indexer_metadata: Vec<NodeMetadata> = indexer_result.map_err(|err| {
        error!("Fatal: failed to get indexer info: {:?}", err);
        PostError::Invalid(err)
    })?;
    // Fetch metrics from ingestors and indexers concurrently
    let (prism_metrics, querier_metrics, ingestor_metrics, indexer_metrics) = future::join4(
        fetch_nodes_metrics(prism_metadata),
        fetch_nodes_metrics(querier_metadata),
        fetch_nodes_metrics(ingestor_metadata),
        fetch_nodes_metrics(indexer_metadata),
    )
    .await;

    // Combine all metrics
    let mut all_metrics = Vec::new();

    // Add prism metrics
    match prism_metrics {
        Ok(metrics) => all_metrics.extend(metrics),
        Err(err) => return Err(err),
    }

    // Add querier metrics
    match querier_metrics {
        Ok(metrics) => all_metrics.extend(metrics),
        Err(err) => return Err(err),
    }

    // Add ingestor metrics
    match ingestor_metrics {
        Ok(metrics) => all_metrics.extend(metrics),
        Err(err) => return Err(err),
    }

    // Add indexer metrics
    match indexer_metrics {
        Ok(metrics) => all_metrics.extend(metrics),
        Err(err) => return Err(err),
    }

    Ok(all_metrics)
}

/// Extracts billing metrics from prometheus samples
fn extract_billing_metrics_from_samples(
    samples: Vec<prometheus_parse::Sample>,
    node_address: String,
    node_type: String,
) -> Vec<BillingMetricEvent> {
    let mut collector = BillingMetricsCollector::new(node_address, node_type);

    for sample in samples {
        if let prometheus_parse::Value::Counter(val) = sample.value {
            process_sample(&mut collector, &sample, val);
        }
    }

    // Convert to flattened events, excluding empty collections
    collector.into_events()
}

/// Process a single prometheus sample and update the collector
fn process_sample(
    collector: &mut BillingMetricsCollector,
    sample: &prometheus_parse::Sample,
    val: f64,
) {
    match sample.metric.as_str() {
        metric if is_simple_metric(metric) => {
            process_simple_metric(collector, metric, &sample.labels, val);
        }
        metric if is_object_store_metric(metric) => {
            process_object_store_metric(collector, metric, &sample.labels, val);
        }
        metric if is_llm_metric(metric) => {
            process_llm_metric(collector, metric, &sample.labels, val);
        }
        _ => {}
    }
}

/// Check if a metric is a simple date-based metric
fn is_simple_metric(metric: &str) -> bool {
    matches!(
        metric,
        "parseable_total_events_ingested_by_date"
            | "parseable_total_events_ingested_size_by_date"
            | "parseable_total_parquets_stored_by_date"
            | "parseable_total_parquets_stored_size_by_date"
            | "parseable_total_query_calls_by_date"
            | "parseable_total_files_scanned_in_query_by_date"
            | "parseable_total_bytes_scanned_in_query_by_date"
    )
}

/// Check if a metric is an object store metric (requires method label)
fn is_object_store_metric(metric: &str) -> bool {
    matches!(
        metric,
        "parseable_total_object_store_calls_by_date"
            | "parseable_total_files_scanned_in_object_store_calls_by_date"
            | "parseable_total_bytes_scanned_in_object_store_calls_by_date"
    )
}

/// Check if a metric is an LLM metric (requires provider and model labels)
fn is_llm_metric(metric: &str) -> bool {
    matches!(
        metric,
        "parseable_total_input_llm_tokens_by_date" | "parseable_total_output_llm_tokens_by_date"
    )
}

/// Process simple metrics that only require a date label
fn process_simple_metric(
    collector: &mut BillingMetricsCollector,
    metric: &str,
    labels: &std::collections::HashMap<String, String>,
    val: f64,
) {
    if let Some(date) = labels.get("date") {
        let value = val as u64;
        match metric {
            "parseable_total_events_ingested_by_date" => {
                collector
                    .total_events_ingested_by_date
                    .insert(date.to_string(), value);
            }
            "parseable_total_events_ingested_size_by_date" => {
                collector
                    .total_events_ingested_size_by_date
                    .insert(date.to_string(), value);
            }
            "parseable_total_parquets_stored_by_date" => {
                collector
                    .total_parquets_stored_by_date
                    .insert(date.to_string(), value);
            }
            "parseable_total_parquets_stored_size_by_date" => {
                collector
                    .total_parquets_stored_size_by_date
                    .insert(date.to_string(), value);
            }
            "parseable_total_query_calls_by_date" => {
                collector
                    .total_query_calls_by_date
                    .insert(date.to_string(), value);
            }
            "parseable_total_files_scanned_in_query_by_date" => {
                collector
                    .total_files_scanned_in_query_by_date
                    .insert(date.to_string(), value);
            }
            "parseable_total_bytes_scanned_in_query_by_date" => {
                collector
                    .total_bytes_scanned_in_query_by_date
                    .insert(date.to_string(), value);
            }
            _ => {}
        }
    }
}

/// Process object store metrics that require method and date labels
fn process_object_store_metric(
    collector: &mut BillingMetricsCollector,
    metric: &str,
    labels: &std::collections::HashMap<String, String>,
    val: f64,
) {
    if let (Some(method), Some(date)) = (labels.get("method"), labels.get("date")) {
        let value = val as u64;
        let target_map = match metric {
            "parseable_total_object_store_calls_by_date" => {
                &mut collector.total_object_store_calls_by_date
            }
            "parseable_total_files_scanned_in_object_store_calls_by_date" => {
                &mut collector.total_files_scanned_in_object_store_calls_by_date
            }
            "parseable_total_bytes_scanned_in_object_store_calls_by_date" => {
                &mut collector.total_bytes_scanned_in_object_store_calls_by_date
            }
            _ => return,
        };

        target_map
            .entry(method.to_string())
            .or_insert_with(HashMap::new)
            .insert(date.to_string(), value);
    }
}

/// Process LLM metrics that require provider, model, and date labels
fn process_llm_metric(
    collector: &mut BillingMetricsCollector,
    metric: &str,
    labels: &std::collections::HashMap<String, String>,
    val: f64,
) {
    if let (Some(provider), Some(model), Some(date)) = (
        labels.get("provider"),
        labels.get("model"),
        labels.get("date"),
    ) {
        let value = val as u64;
        let target_map = match metric {
            "parseable_total_input_llm_tokens_by_date" => {
                &mut collector.total_input_llm_tokens_by_date
            }
            "parseable_total_output_llm_tokens_by_date" => {
                &mut collector.total_output_llm_tokens_by_date
            }
            _ => return,
        };

        target_map
            .entry(provider.to_string())
            .or_insert_with(HashMap::new)
            .entry(model.to_string())
            .or_insert_with(HashMap::new)
            .insert(date.to_string(), value);
    }
}

/// Fetches billing metrics for a single node
async fn fetch_node_billing_metrics<T>(node: &T) -> Result<Vec<BillingMetricEvent>, PostError>
where
    T: Metadata + Send + Sync + 'static,
{
    // Format the metrics URL
    let uri = Url::parse(&format!(
        "{}{}/metrics",
        node.domain_name(),
        base_path_without_preceding_slash()
    ))
    .map_err(|err| PostError::Invalid(anyhow::anyhow!("Invalid URL in node metadata: {}", err)))?;

    // Check if the node is live
    if !check_liveness(node.domain_name()).await {
        warn!("node {} is not live", node.domain_name());
        return Ok(Vec::new());
    }

    // Fetch metrics
    let res = INTRA_CLUSTER_CLIENT
        .get(uri)
        .header(header::AUTHORIZATION, node.token())
        .header(header::CONTENT_TYPE, "application/json")
        .send()
        .await;

    match res {
        Ok(res) => {
            let text = res.text().await.map_err(PostError::NetworkError)?;
            let lines: Vec<Result<String, std::io::Error>> =
                text.lines().map(|line| Ok(line.to_owned())).collect_vec();

            let sample = prometheus_parse::Scrape::parse(lines.into_iter())
                .map_err(|err| PostError::CustomError(err.to_string()))?
                .samples;

            let billing_metrics = extract_billing_metrics_from_samples(
                sample,
                node.domain_name().to_string(),
                node.node_type().to_string(),
            );

            Ok(billing_metrics)
        }
        Err(_) => {
            warn!(
                "Failed to fetch billing metrics from node: {}\n",
                node.domain_name()
            );
            Ok(Vec::new())
        }
    }
}

/// Fetches billing metrics from multiple nodes in parallel
async fn fetch_nodes_billing_metrics<T>(nodes: Vec<T>) -> Result<Vec<BillingMetricEvent>, PostError>
where
    T: Metadata + Send + Sync + 'static,
{
    let nodes_len = nodes.len();
    if nodes_len == 0 {
        return Ok(vec![]);
    }

    let results = stream::iter(nodes)
        .map(|node| async move { fetch_node_billing_metrics(&node).await })
        .buffer_unordered(nodes_len) // No concurrency limit
        .collect::<Vec<_>>()
        .await;

    // Collect results, filtering out errors and flattening events
    let mut billing_metrics = Vec::new();
    for result in results {
        match result {
            Ok(metrics) => billing_metrics.extend(metrics), // Flatten all events from all nodes
            Err(err) => {
                error!("Error fetching billing metrics: {:?}", err);
                // Continue with other nodes instead of failing the entire operation
            }
        }
    }

    Ok(billing_metrics)
}

/// Main function to fetch billing metrics from all nodes
async fn fetch_cluster_billing_metrics() -> Result<Vec<BillingMetricEvent>, PostError> {
    // Get all node types metadata concurrently
    let (prism_result, querier_result, ingestor_result, indexer_result) = future::join4(
        get_node_info(NodeType::Prism),
        get_node_info(NodeType::Querier),
        get_node_info(NodeType::Ingestor),
        get_node_info(NodeType::Indexer),
    )
    .await;

    // Handle results
    let prism_metadata: Vec<NodeMetadata> = prism_result.map_err(|err| {
        error!("Failed to get prism info for billing metrics: {:?}", err);
        PostError::Invalid(err)
    })?;

    let querier_metadata: Vec<NodeMetadata> = querier_result.map_err(|err| {
        error!("Failed to get querier info for billing metrics: {:?}", err);
        PostError::Invalid(err)
    })?;

    let ingestor_metadata: Vec<NodeMetadata> = ingestor_result.map_err(|err| {
        error!("Failed to get ingestor info for billing metrics: {:?}", err);
        PostError::Invalid(err)
    })?;

    let indexer_metadata: Vec<NodeMetadata> = indexer_result.map_err(|err| {
        error!("Failed to get indexer info for billing metrics: {:?}", err);
        PostError::Invalid(err)
    })?;

    // Fetch billing metrics from all nodes concurrently
    let (prism_metrics, querier_metrics, ingestor_metrics, indexer_metrics) = future::join4(
        fetch_nodes_billing_metrics(prism_metadata),
        fetch_nodes_billing_metrics(querier_metadata),
        fetch_nodes_billing_metrics(ingestor_metadata),
        fetch_nodes_billing_metrics(indexer_metadata),
    )
    .await;

    // Combine all billing metrics
    let mut all_billing_metrics = Vec::new();

    // Add metrics from all node types
    match prism_metrics {
        Ok(metrics) => all_billing_metrics.extend(metrics),
        Err(err) => error!("Error fetching prism billing metrics: {:?}", err),
    }

    match querier_metrics {
        Ok(metrics) => all_billing_metrics.extend(metrics),
        Err(err) => error!("Error fetching querier billing metrics: {:?}", err),
    }

    match ingestor_metrics {
        Ok(metrics) => all_billing_metrics.extend(metrics),
        Err(err) => error!("Error fetching ingestor billing metrics: {:?}", err),
    }

    match indexer_metrics {
        Ok(metrics) => all_billing_metrics.extend(metrics),
        Err(err) => error!("Error fetching indexer billing metrics: {:?}", err),
    }

    Ok(all_billing_metrics)
}

pub fn init_cluster_metrics_schedular() -> Result<(), PostError> {
    info!("Setting up schedular for cluster metrics ingestion");
    let mut scheduler = AsyncScheduler::new();
    scheduler
        .every(CLUSTER_METRICS_INTERVAL_SECONDS)
        .run(move || async {
            let result: Result<(), PostError> = async {
                // Fetch regular cluster metrics
                let cluster_metrics = fetch_cluster_metrics().await;
                if let Ok(metrics) = cluster_metrics
                    && !metrics.is_empty()
                {
                    info!("Cluster metrics fetched successfully from all nodes");
                    if let Ok(metrics_bytes) = serde_json::to_vec(&metrics) {
                        if matches!(
                            ingest_internal_stream(
                                PMETA_STREAM_NAME.to_string(),
                                bytes::Bytes::from(metrics_bytes),
                            )
                            .await,
                            Ok(())
                        ) {
                            info!("Cluster metrics successfully ingested into internal stream");
                        } else {
                            error!("Failed to ingest cluster metrics into internal stream");
                        }
                    } else {
                        error!("Failed to serialize cluster metrics");
                    }
                }

                // Fetch billing metrics
                match fetch_cluster_billing_metrics().await {
                    Ok(metrics) if !metrics.is_empty() => {
                        info!("Billing metrics fetched successfully from all nodes");
                        // Optionally add: trace!("Billing metrics: {:?}", metrics);
                        if let Ok(billing_metrics_bytes) = serde_json::to_vec(&metrics) {
                            if matches!(
                                ingest_internal_stream(
                                    BILLING_METRICS_STREAM_NAME.to_string(),
                                    bytes::Bytes::from(billing_metrics_bytes),
                                )
                                .await,
                                Ok(())
                            ) {
                                info!("Billing metrics successfully ingested into billing-metrics stream");
                            } else {
                                error!("Failed to ingest billing metrics into billing-metrics stream");
                            }
                        } else {
                            error!("Failed to serialize billing metrics");
                        }
                    }
                    Ok(_) => {
                        // Empty metrics result
                        info!("No billing metrics to ingest (empty result)");
                    }
                    Err(err) => {
                        error!("Error fetching billing metrics: {:?}", err);
                    }
                }

                Ok(())
            }
            .await;

            if let Err(err) = result {
                error!("Error in cluster metrics scheduler: {:?}", err);
            }
        });

    tokio::spawn(async move {
        loop {
            scheduler.run_pending().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    Ok(())
}

#[derive(Clone, Debug)]
struct QuerierStatus {
    metadata: QuerierMetadata,
    available: bool,
    last_used: Option<Instant>,
}

pub async fn get_available_querier() -> Result<QuerierMetadata, QueryError> {
    // Get all querier metadata
    let querier_metadata: Vec<NodeMetadata> = get_node_info(NodeType::Querier).await?;

    // No queriers found
    if querier_metadata.is_empty() {
        return Err(QueryError::NoAvailableQuerier);
    }

    // Limit concurrency for liveness checks to avoid resource exhaustion
    const MAX_CONCURRENT_LIVENESS_CHECKS: usize = 10;
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_LIVENESS_CHECKS));

    // Update the querier map with new metadata and get an available querier
    let mut map = QUERIER_MAP.write().await;

    let existing_domains: Vec<String> = map.keys().cloned().collect();
    let mut live_domains = std::collections::HashSet::new();

    // Use stream with concurrency limit instead of join_all
    let liveness_results: Vec<(String, bool, NodeMetadata)> = stream::iter(querier_metadata)
        .map(|metadata| {
            let domain = metadata.domain_name.clone();
            let metadata_clone = metadata.clone();
            let semaphore = Arc::clone(&semaphore);

            async move {
                let _permit = semaphore.acquire().await.unwrap();
                let is_live = check_liveness(&domain).await;
                (domain, is_live, metadata_clone)
            }
        })
        .buffer_unordered(MAX_CONCURRENT_LIVENESS_CHECKS)
        .collect()
        .await;

    // Update the map based on liveness results
    for (domain, is_live, metadata) in liveness_results {
        if is_live {
            live_domains.insert(domain.clone());
            // Update existing entry or add new one
            if let Some(status) = map.get_mut(&domain) {
                // Update metadata for existing entry, preserve last_used
                status.metadata = metadata;
            } else {
                // Add new entry
                map.insert(
                    domain,
                    QuerierStatus {
                        metadata,
                        available: true,
                        last_used: None,
                    },
                );
            }
        }
    }

    // Remove entries that are not live anymore
    existing_domains.iter().for_each(|domain| {
        if !live_domains.contains(domain) {
            map.remove(domain);
        }
    });

    // Find the next available querier using round-robin strategy
    if let Some(selected_domain) = select_next_querier(&mut map).await
        && let Some(status) = map.get_mut(&selected_domain)
    {
        status.available = false;
        status.last_used = Some(Instant::now());
        return Ok(status.metadata.clone());
    }

    // If no querier is available, use least-recently-used strategy
    if let Some(selected_domain) = select_least_recently_used_querier(&mut map)
        && let Some(status) = map.get_mut(&selected_domain)
    {
        status.available = false;
        status.last_used = Some(Instant::now());
        return Ok(status.metadata.clone());
    }

    // If no querier is available, return an error
    Err(QueryError::NoAvailableQuerier)
}

/// Select next querier using round-robin strategy
async fn select_next_querier(map: &mut HashMap<String, QuerierStatus>) -> Option<String> {
    // First, try to find any available querier
    let available_queriers: Vec<String> = map
        .iter()
        .filter_map(|(domain, status)| {
            if status.available {
                Some(domain.clone())
            } else {
                None
            }
        })
        .collect();

    if available_queriers.is_empty() {
        return None;
    }

    // Get the last used querier for round-robin
    let last_used = LAST_USED_QUERIER.read().await;

    if let Some(ref last_domain) = *last_used {
        // Find the next querier in the list after the last used one
        let mut found_last = false;
        for domain in &available_queriers {
            if found_last {
                drop(last_used);
                *LAST_USED_QUERIER.write().await = Some(domain.clone());
                return Some(domain.clone());
            }
            if domain == last_domain {
                found_last = true;
            }
        }
        // If we reached here, either last_used querier is not available anymore
        // or it was the last in the list, so wrap around to the first
        if let Some(first_domain) = available_queriers.first() {
            drop(last_used);
            *LAST_USED_QUERIER.write().await = Some(first_domain.clone());
            return Some(first_domain.clone());
        }
    } else {
        // No previous querier, select the first available one
        if let Some(first_domain) = available_queriers.first() {
            drop(last_used);
            *LAST_USED_QUERIER.write().await = Some(first_domain.clone());
            return Some(first_domain.clone());
        }
    }

    None
}

/// Select the least recently used querier when no querier is marked as available
fn select_least_recently_used_querier(map: &mut HashMap<String, QuerierStatus>) -> Option<String> {
    if map.is_empty() {
        return None;
    }

    // Find the querier that was used least recently (or never used)
    let mut least_recently_used_domain: Option<String> = None;
    let mut oldest_time: Option<Instant> = None;

    for (domain, status) in map.iter() {
        match (status.last_used, oldest_time) {
            // Never used - highest priority
            (None, _) => {
                least_recently_used_domain = Some(domain.clone());
                oldest_time = None;
            }
            // Used, but we haven't found any used querier yet
            (Some(used_time), None) => {
                if least_recently_used_domain.is_none() {
                    least_recently_used_domain = Some(domain.clone());
                    oldest_time = Some(used_time);
                }
            }
            // Used, and we have a candidate - compare times
            (Some(used_time), Some(current_oldest)) => {
                if used_time < current_oldest {
                    least_recently_used_domain = Some(domain.clone());
                    oldest_time = Some(used_time);
                }
            }
        }
    }

    least_recently_used_domain
}

// Mark a querier as available again
pub async fn mark_querier_available(domain_name: &str) {
    let mut map = QUERIER_MAP.write().await;
    if let Some(status) = map.get_mut(domain_name) {
        status.available = true;
        // Note: We don't reset last_used here as it's used for LRU selection
    }
}

pub async fn send_query_request(query_request: &Query) -> Result<(JsonValue, String), QueryError> {
    let querier = get_available_querier().await?;
    let domain_name = querier.domain_name.clone();

    // Perform the query request
    let fields = query_request.fields;
    let streaming = query_request.streaming;
    let send_null = query_request.send_null;
    let uri = format!(
        "{}api/v1/query?fields={fields}&streaming={streaming}&send_null={send_null}",
        &querier.domain_name,
    );

    let body = match serde_json::to_string(&query_request) {
        Ok(body) => body,
        Err(err) => {
            mark_querier_available(&domain_name).await;
            return Err(QueryError::from(err));
        }
    };

    let res = match INTRA_CLUSTER_CLIENT
        .post(uri)
        .timeout(Duration::from_secs(300))
        .header(header::AUTHORIZATION, &querier.token)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(res) => res,
        Err(err) => {
            mark_querier_available(&domain_name).await;
            return Err(QueryError::from(err));
        }
    };

    // Mark querier as available immediately after the HTTP request completes
    mark_querier_available(&domain_name).await;

    let headers = res.headers();
    let total_time = match headers.get(TIME_ELAPSED_HEADER) {
        Some(v) => {
            let total_time = v.to_str().unwrap_or_default();
            total_time.to_string()
        }
        None => String::default(),
    };

    if res.status().is_success() {
        match res.text().await {
            Ok(text) => {
                let query_response: JsonValue = serde_json::from_str(&text)?;
                Ok((query_response, total_time))
            }
            Err(err) => {
                error!("Error parsing query response: {:?}", err);
                Err(QueryError::Anyhow(err.into()))
            }
        }
    } else {
        let err_text = res.text().await?;
        Err(QueryError::JsonParse(err_text))
    }
}
