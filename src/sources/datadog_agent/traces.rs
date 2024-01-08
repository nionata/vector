use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use chrono::{TimeZone, Utc};
use futures::future;
use http::StatusCode;
use ordered_float::NotNan;
use protobuf::{Chars, Message as _};
use vrl::event_path;
use warp::{filters::BoxedFilter, path, path::FullPath, reply::Response, Filter, Rejection, Reply};

use vector_lib::internal_event::{CountByteSize, InternalEventHandle as _};
use vector_lib::EstimatedJsonEncodedSizeOf;

use crate::{
    event::{Event, ObjectMap, TraceEvent, Value},
    sources::{
        datadog_agent::{
            handle_request,
            proto::traces::{Span, TracePayload, TracerPayload},
            ApiKeyQueryParams, DatadogAgentSource,
        },
        util::ErrorMessage,
    },
    SourceSender,
};

pub(crate) fn build_warp_filter(
    acknowledgements: bool,
    multiple_outputs: bool,
    out: SourceSender,
    source: DatadogAgentSource,
) -> BoxedFilter<(Response,)> {
    build_trace_filter(acknowledgements, multiple_outputs, out, source)
        .or(build_stats_filter())
        .unify()
        .boxed()
}

fn build_trace_filter(
    acknowledgements: bool,
    multiple_outputs: bool,
    out: SourceSender,
    source: DatadogAgentSource,
) -> BoxedFilter<(Response,)> {
    warp::post()
        .and(path!("api" / "v0.2" / "traces" / ..))
        .and(warp::path::full())
        .and(warp::header::optional::<String>("content-encoding"))
        .and(warp::header::optional::<String>("dd-api-key"))
        .and(warp::header::optional::<String>(
            "X-Datadog-Reported-Languages",
        ))
        .and(warp::query::<ApiKeyQueryParams>())
        .and(warp::body::bytes())
        .and_then(
            move |path: FullPath,
                  encoding_header: Option<String>,
                  api_token: Option<String>,
                  reported_language: Option<String>,
                  query_params: ApiKeyQueryParams,
                  body: Bytes| {
                let events = source
                    .decode(&encoding_header, body, path.as_str())
                    .and_then(|body| {
                        handle_dd_trace_payload(
                            body,
                            source.api_key_extractor.extract(
                                path.as_str(),
                                api_token,
                                query_params.dd_api_key,
                            ),
                            reported_language.as_ref(),
                            &source,
                        )
                        .map_err(|error| {
                            ErrorMessage::new(
                                StatusCode::UNPROCESSABLE_ENTITY,
                                format!("Error decoding Datadog traces: {:?}", error),
                            )
                        })
                    });
                let output = multiple_outputs.then_some(super::TRACES);
                handle_request(events, acknowledgements, out.clone(), output)
            },
        )
        .boxed()
}

fn build_stats_filter() -> BoxedFilter<(Response,)> {
    warp::post()
        .and(path!("api" / "v0.2" / "stats" / ..))
        .and_then(|| {
            // APM stats are discarded on purpose, they will be computed in the `datadog_traces` sink
            // thus we simply reply with a 200/OK response.
            let response: Result<Response, Rejection> = Ok(warp::reply().into_response());
            future::ready(response)
        })
        .boxed()
}

fn handle_dd_trace_payload(
    frame: Bytes,
    api_key: Option<Arc<str>>,
    lang: Option<&String>,
    source: &DatadogAgentSource,
) -> crate::Result<Vec<Event>> {
    let decoded_payload = TracePayload::parse_from_tokio_bytes(&frame)?;
    if decoded_payload.tracerPayloads.is_empty() {
        debug!("Older trace payload decoded.");
        handle_dd_trace_payload_v0(decoded_payload, api_key, lang, source)
    } else {
        debug!("Newer trace payload decoded.");
        handle_dd_trace_payload_v1(decoded_payload, api_key, source)
    }
}

/// Decode Datadog newer protobuf schema
fn handle_dd_trace_payload_v1(
    decoded_payload: TracePayload,
    api_key: Option<Arc<str>>,
    source: &DatadogAgentSource,
) -> crate::Result<Vec<Event>> {
    // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
    // solution for getting original `Bytes` from `Chars`
    let env = decoded_payload.env.to_string();
    let hostname = decoded_payload.hostName.to_string();
    let agent_version = decoded_payload.agentVersion.to_string();
    let target_tps = decoded_payload.targetTPS;
    let error_tps = decoded_payload.errorTPS;
    let tags = convert_tags(decoded_payload.tags);

    let trace_events: Vec<TraceEvent> = decoded_payload
        .tracerPayloads
        .into_iter()
        .flat_map(convert_dd_tracer_payload)
        .collect();

    source.events_received.emit(CountByteSize(
        trace_events.len(),
        trace_events.estimated_json_encoded_size_of(),
    ));

    let enriched_events = trace_events
        .into_iter()
        .map(|mut trace_event| {
            if let Some(k) = &api_key {
                trace_event
                    .metadata_mut()
                    .set_datadog_api_key(Arc::clone(k));
            }
            trace_event.insert(
                &source.log_schema_source_type_key,
                Bytes::from("datadog_agent"),
            );
            trace_event.insert(event_path!("payload_version"), "v2".to_string());
            trace_event.insert(&source.log_schema_host_key, hostname.clone());
            trace_event.insert(event_path!("env"), env.clone());
            trace_event.insert(event_path!("agent_version"), agent_version.clone());
            trace_event.insert(
                event_path!("target_tps"),
                Value::Float(NotNan::new(target_tps).expect("target_tps cannot be Nan")),
            );
            trace_event.insert(
                event_path!("error_tps"),
                Value::Float(NotNan::new(error_tps).expect("error_tps cannot be Nan")),
            );
            if let Some(Value::Object(span_tags)) = trace_event.get_mut(event_path!("tags")) {
                span_tags.extend(tags.clone());
            } else {
                trace_event.insert(event_path!("tags"), Value::from(tags.clone()));
            }
            Event::Trace(trace_event)
        })
        .collect();
    Ok(enriched_events)
}

fn convert_dd_tracer_payload(payload: TracerPayload) -> Vec<TraceEvent> {
    let tags = convert_tags(payload.tags);
    payload
        .chunks
        .into_iter()
        .map(|trace| {
            let mut trace_event = TraceEvent::default();
            trace_event.insert(event_path!("priority"), trace.priority as i64);
            // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
            // solution for getting original `Bytes` from `Chars`
            trace_event.insert(event_path!("origin"), trace.origin.to_string());
            trace_event.insert(event_path!("dropped"), trace.droppedTrace);
            let mut trace_tags = convert_tags(trace.tags);
            trace_tags.extend(tags.clone());
            trace_event.insert(event_path!("tags"), Value::from(trace_tags));

            trace_event.insert(
                event_path!("spans"),
                trace
                    .spans
                    .into_iter()
                    .map(|s| Value::from(convert_span(s)))
                    .collect::<Vec<Value>>(),
            );

            // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
            // solution for getting original `Bytes` from `Chars`
            trace_event.insert(event_path!("container_id"), payload.containerID.to_string());
            trace_event.insert(
                event_path!("language_name"),
                payload.languageName.to_string(),
            );
            trace_event.insert(
                event_path!("language_version"),
                payload.languageVersion.to_string(),
            );
            trace_event.insert(
                event_path!("tracer_version"),
                payload.tracerVersion.to_string(),
            );
            trace_event.insert(event_path!("runtime_id"), payload.runtimeID.to_string());
            trace_event.insert(event_path!("app_version"), payload.appVersion.to_string());
            trace_event
        })
        .collect()
}

// Decode Datadog older protobuf schema
fn handle_dd_trace_payload_v0(
    decoded_payload: TracePayload,
    api_key: Option<Arc<str>>,
    lang: Option<&String>,
    source: &DatadogAgentSource,
) -> crate::Result<Vec<Event>> {
    // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
    // solution for getting original `Bytes` from `Chars`
    let env = decoded_payload.env.to_string();
    let hostname = decoded_payload.hostName.to_string();

    let trace_events: Vec<TraceEvent> =
    // Each traces is mapped to one event...
    decoded_payload
        .traces
        .into_iter()
        .map(|dd_trace| {
            let mut trace_event = TraceEvent::default();

            // TODO trace_id is being forced into an i64 but
            // the incoming payload is u64. This is a bug and needs to be fixed per:
            // https://github.com/vectordotdev/vector/issues/14687
            trace_event.insert(event_path!("trace_id"), dd_trace.traceID as i64);
            trace_event.insert(event_path!("start_time"), Utc.timestamp_nanos(dd_trace.startTime));
            trace_event.insert(event_path!("end_time"), Utc.timestamp_nanos(dd_trace.endTime));
            trace_event.insert(
                event_path!("spans"),
                dd_trace
                    .spans
                    .into_iter()
                    .map(|s| Value::from(convert_span(s)))
                    .collect::<Vec<Value>>(),
            );
            trace_event
        })
        //... and each APM event is also mapped into its own event
        .chain(decoded_payload.transactions.into_iter().map(|s| {
            let mut trace_event = TraceEvent::default();
            trace_event.insert(event_path!("spans"), vec![Value::from(convert_span(s))]);
            trace_event.insert(event_path!("dropped"), true);
            trace_event
        })).collect();

    source.events_received.emit(CountByteSize(
        trace_events.len(),
        trace_events.estimated_json_encoded_size_of(),
    ));

    let enriched_events = trace_events
        .into_iter()
        .map(|mut trace_event| {
            if let Some(k) = &api_key {
                trace_event
                    .metadata_mut()
                    .set_datadog_api_key(Arc::clone(k));
            }
            if let Some(lang) = lang {
                trace_event.insert(event_path!("language_name"), lang.clone());
            }
            trace_event.insert(
                &source.log_schema_source_type_key,
                Bytes::from("datadog_agent"),
            );
            trace_event.insert(event_path!("payload_version"), "v1".to_string());
            trace_event.insert(&source.log_schema_host_key, hostname.clone());
            trace_event.insert(event_path!("env"), env.clone());
            Event::Trace(trace_event)
        })
        .collect();

    Ok(enriched_events)
}

fn convert_span(dd_span: Span) -> ObjectMap {
    let mut span = ObjectMap::new();

    // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
    // solution for getting original `Bytes` from `Chars`
    span.insert("service".into(), Value::from(dd_span.service.to_string()));
    span.insert("name".into(), Value::from(dd_span.name.to_string()));
    span.insert("resource".into(), Value::from(dd_span.resource.to_string()));

    span.insert("trace_id".into(), Value::from(dd_span.traceID));
    span.insert("span_id".into(), Value::from(dd_span.spanID));
    span.insert("parent_id".into(), Value::from(dd_span.parentID));
    span.insert(
        "start".into(),
        Value::from(Utc.timestamp_nanos(dd_span.start)),
    );
    span.insert("duration".into(), Value::from(dd_span.duration));
    span.insert("error".into(), Value::from(dd_span.error as i64));
    span.insert("meta".into(), Value::from(convert_tags(dd_span.meta)));
    span.insert(
        "metrics".into(),
        Value::from(
            dd_span
                .metrics
                .into_iter()
                .map(|(k, v)| {
                    (
                        k.to_string().into(),
                        NotNan::new(v).map(Value::Float).unwrap_or(Value::Null),
                    )
                })
                .collect::<ObjectMap>(),
        ),
    );
    // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
    // solution for getting original `Bytes` from `Chars`
    span.insert("type".into(), Value::from(dd_span.type_.to_string()));
    span.insert(
        "meta_struct".into(),
        Value::from(
            dd_span
                .meta_struct
                .into_iter()
                .map(|(k, v)| (k.to_string().into(), Value::from(bytes::Bytes::from(v))))
                .collect::<ObjectMap>(),
        ),
    );

    span
}

fn convert_tags(original_map: HashMap<Chars, Chars>) -> ObjectMap {
    original_map
        .into_iter()
        // TODO: inefficient conversion to `String` when `Bytes` would suffice. address once we have
        // solution for getting original `Bytes` from `Chars`
        .map(|(k, v)| (k.to_string().into(), Value::from(v.to_string())))
        .collect::<ObjectMap>()
}
