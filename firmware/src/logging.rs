//! Off-box logging plane (evc04#3): `tracing` is the facade the whole firmware
//! writes to, and every record leaves the box as an OTLP log record over
//! HTTP/protobuf.
//!
//! Why this exists: on 2026-09-02 the box latched a solid red fault for ~10 h
//! while every parsed telemetry field read healthy. The only witness would have
//! been the raw CN28 lines — and those were kept nowhere. They are now log
//! records, with the forensic hex dump attached to exactly the lines that fail
//! to parse.
//!
//! ⚠️ Two invariants keep this safe on a box that must never stop answering the
//! meter poll:
//!   - **Nothing here may block a caller.** `BatchLogProcessor::emit` is a
//!     `try_send` onto a bounded queue: when the queue is full or the collector
//!     is unreachable, records are *dropped*, never queued into the control tick.
//!     All network work happens on the SDK's own exporter thread.
//!   - **No telemetry-induced telemetry.** The exporter thread runs inside the
//!     SDK's telemetry-suppressed scope, so the `log` chatter our own HTTP client
//!     emits while shipping a batch cannot become another batch.
//!
//! Timestamps come from the system clock, which SNTP corrects a few seconds
//! after boot ([`start_clock_sync`]); records emitted before that carry a
//! pre-sync (1970) timestamp rather than delaying the workers' start.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::sntp::EspSntp;
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{LogExporter, Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::{BatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider};
use opentelemetry_sdk::Resource;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Full OTLP logs endpoint, baked in at build time like the broker URL, e.g.
/// `http://collector.lan:4318/v1/logs`. The exporter uses it verbatim — it is
/// the signal endpoint, not the collector's base URL.
const OTLP_LOGS_URL: &str = env!("OTLP_LOGS_URL");
/// Optional `Authorization` header value for the collector, e.g. `Basic <b64>`.
/// Optional because a collector on the trusted LAN may take unauthenticated
/// posts; a build that leaves it unset simply sends no header.
const OTLP_LOGS_AUTH: Option<&str> = option_env!("OTLP_LOGS_AUTH");
/// Log stream the records land in. Collectors that key their storage off a
/// header (OpenObserve does) need it; the ones that don't ignore it.
const OTLP_LOGS_STREAM: &str = "evc04";
/// Baked by `build.rs`, same string the version topic publishes.
const FW_VERSION: &str = env!("FW_VERSION");

/// Records buffered while the collector is unreachable. Sized for RAM, not for
/// completeness: at the ~10 records/s the 2 s probe cadence produces this holds
/// roughly half a minute, and anything older is dropped rather than crowding out
/// the control path. Nothing survives a reboot on purpose — the box has 4 MB of
/// flash carrying two OTA slots, and the reboot itself is already reported
/// (`reset_reason` on the version topic, plus the sticky CN28 fault).
const MAX_QUEUE: usize = 256;
/// Records per export. One batch of full CN28 lines is a few kB of protobuf.
const MAX_BATCH: usize = 64;
/// Export at least this often, so a quiet box still reports within the window.
const SCHEDULED_DELAY: Duration = Duration::from_secs(5);
/// Bounds one export end to end. Runs on the exporter thread, so a hung
/// collector costs records, never the control tick — but it must still fail
/// fast enough that the next batch is not stuck behind it.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Response body we bother to read: the collector answers OTLP/HTTP success with
/// an empty `ExportLogsServiceResponse`, and a longer error body is truncated.
const RESPONSE_BUF: usize = 512;

/// Install the tracing subscriber and return the provider that owns the exporter
/// thread — the caller must keep it alive for the life of the process.
///
/// Both sinks are wired here: the serial console (so a USB monitor still shows
/// everything) and the OTLP bridge. `log`-crate records — everything esp-idf-svc
/// and esp-idf-hal emit — are routed in as well, so "off-box logging" really
/// means all of it and not just our own call sites.
pub fn init() -> Result<SdkLoggerProvider> {
    let exporter = LogExporter::builder()
        .with_http()
        .with_endpoint(OTLP_LOGS_URL)
        .with_protocol(Protocol::HttpBinary)
        .with_timeout(EXPORT_TIMEOUT)
        .with_headers(headers())
        .with_http_client(EspHttpClient)
        .build()
        .context("otlp log exporter")?;

    let processor = BatchLogProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(MAX_QUEUE)
                .with_max_export_batch_size(MAX_BATCH)
                .with_scheduled_delay(SCHEDULED_DELAY)
                .build(),
        )
        .build();

    let provider = SdkLoggerProvider::builder()
        .with_resource(resource())
        .with_log_processor(processor)
        .build();

    // `try_init`, never `init`: the latter unwraps and panics, and a panic here
    // happens on *every* boot — 34 reboots in 30 s when this went wrong (#3). A
    // logging plane must not be able to brick the box, so the error travels up to
    // `main`, which carries on without logging.
    //
    // The `log` bridge is NOT installed by hand: with tracing-subscriber's
    // `tracing-log` feature, `try_init` sets up the log compatibility layer
    // itself. Calling `LogTracer::init()` first is what made that fail here —
    // the second registration returns `SetLoggerError`.
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(tracing_subscriber::fmt::layer().with_ansi(false))
        .with(OpenTelemetryTracingBridge::new(&provider))
        .try_init()
        .context("install tracing subscriber")?;

    Ok(provider)
}

/// Start SNTP so log timestamps become real wall-clock time. The handle must be
/// held for the life of the process; syncing is asynchronous and never blocks
/// the boot path — the workers (and with them the meter poll) start regardless.
pub fn start_clock_sync() -> Result<EspSntp<'static>> {
    EspSntp::new_default().context("sntp init")
}

/// Headers every export carries: the stream to land in, and the collector
/// credentials when the build was given any.
fn headers() -> HashMap<String, String> {
    let mut headers = HashMap::from([("stream-name".to_owned(), OTLP_LOGS_STREAM.to_owned())]);
    if let Some(auth) = OTLP_LOGS_AUTH.filter(|a| !a.is_empty()) {
        headers.insert("Authorization".to_owned(), auth.to_owned());
    }
    headers
}

/// Identity every record carries: which firmware, on which board.
fn resource() -> Resource {
    Resource::builder()
        .with_service_name("evc04-cn28-prober")
        .with_attributes([
            KeyValue::new("service.version", FW_VERSION),
            KeyValue::new("service.instance.id", instance_id()),
        ])
        .build()
}

/// The board's factory MAC as `aabbccddeeff` — stable across reflashes and OTA,
/// so records from one box stay one series even after a firmware change.
fn instance_id() -> String {
    let mut mac = [0u8; 6];
    // esp-idf writes exactly 6 bytes; the call only fails on a blank eFuse block,
    // in which case the all-zero id is still a usable (if unhelpful) identity.
    unsafe { esp_idf_svc::sys::esp_efuse_mac_get_default(mac.as_mut_ptr()) };
    mac.iter().map(|b| format!("{b:02x}")).collect()
}

/// The OTLP exporter's HTTP transport, on esp-idf's own client.
///
/// A fresh connection per export is deliberate: batches are seconds apart, so a
/// pooled socket would sit idle far longer than any NAT or collector keeps it,
/// and a stale-socket retry costs more than the connect it saves.
#[derive(Debug)]
struct EspHttpClient;

#[async_trait]
impl HttpClient for EspHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        // Blocking I/O inside an `async fn`: the SDK drives this with
        // `futures_executor::block_on` on its own exporter thread — the same
        // shape opentelemetry-http's blocking reqwest client uses.
        post(request).map_err(Into::into)
    }
}

fn post(request: Request<Bytes>) -> Result<Response<Bytes>> {
    let mut conn = EspHttpConnection::new(&HttpConfig {
        timeout: Some(EXPORT_TIMEOUT),
        ..Default::default()
    })
    .context("http client init")?;

    let uri = request.uri().to_string();
    let body = request.body();
    let content_length = body.len().to_string();
    let mut headers: Vec<(&str, &str)> = Vec::with_capacity(request.headers().len() + 1);
    for (name, value) in request.headers() {
        headers.push((name.as_str(), value.to_str().context("header value")?));
    }
    headers.push(("content-length", content_length.as_str()));

    conn.initiate_request(Method::Post, &uri, &headers)
        .context("http POST")?;
    conn.write_all(body).context("http write")?;
    conn.flush().context("http flush")?;
    conn.initiate_response().context("http response")?;

    let status = conn.status();
    let mut buf = [0u8; RESPONSE_BUF];
    let mut received = Vec::new();
    loop {
        let n = conn.read(&mut buf).context("http read")?;
        if n == 0 || received.len() >= RESPONSE_BUF {
            break;
        }
        received.extend_from_slice(&buf[..n]);
    }

    Response::builder()
        .status(status)
        .body(Bytes::from(received))
        .context("http response build")
}
