use std::{
    future::IntoFuture,
    ops::Deref,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use anyhow::bail;
use arc_swap::ArcSwapOption;
use axum::{
    extract::{FromRequestParts, Path, State},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json, Router,
};
use enum_map::EnumMap;
use serde_json::{json, Map, Value};
use tokio::{
    net::TcpListener,
    sync::{broadcast, watch},
    time::Instant,
};
use tracing::info;
use typed_path::Utf8Component;

use crate::{
    config::ServerConfig,
    printer_spy::{PrintStateCode, PrinterEvent, Thermistor},
    slurper::{CurrentGcode, Gcode, PathBuf},
    Critical,
};

type StaticOutput = ArcSwapOption<Value>;

pub async fn refresh_api_printer(
    mut event_stream_in: broadcast::Receiver<PrinterEvent>,
    target: &StaticOutput,
) -> anyhow::Result<Critical> {
    let mut printer_state_text = "Offline";
    let mut printer_state_code = PrintStateCode::Offline;

    let mut temperatures: EnumMap<_, (_, _)> = EnumMap::default();

    loop {
        let target_state = json!({
            "text": printer_state_text,
            "flags": {
                "operational": printer_state_code != PrintStateCode::Offline,
                "paused": printer_state_code == PrintStateCode::Paused,
                "printing": printer_state_code == PrintStateCode::Printing,
                "pausing": false,
                "cancelling": false,
                "sdReady": false,
                "error": false,
                "ready": printer_state_code == PrintStateCode::Idle,
                "closedOrError": printer_state_code == PrintStateCode::Offline
            }
        });

        let mut next = Map::new();
        next.insert("state".into(), target_state);

        if printer_state_code != PrintStateCode::Offline {
            next.insert(
                "temperature".into(),
                json!({
                    "tool0": {
                        "actual": temperatures[Thermistor::Tool].0,
                        "target": temperatures[Thermistor::Tool].1,
                        "offset": 0
                    },
                    "bed": {
                        "actual": temperatures[Thermistor::Bed].0,
                        "target": temperatures[Thermistor::Bed].1,
                        "offset": 0
                    }
                }),
            );
        }

        target.store(Some(Arc::new(Value::Object(next))));

        match event_stream_in.recv().await? {
            PrinterEvent::PrintState { raw, code } => {
                printer_state_text = raw;
                printer_state_code = code;
            }
            PrinterEvent::Temperature { which, value } => {
                temperatures[which].0 = value;
            }
            PrinterEvent::TemperatureTarget { which, value } => {
                temperatures[which].1 = value;
            }
            _ => {
                continue;
            }
        }
    }
}

pub async fn refresh_api_job(
    mut event_stream_in: broadcast::Receiver<PrinterEvent>,
    mut gcode_stream_in: watch::Receiver<CurrentGcode>,
    target: &StaticOutput,
) -> anyhow::Result<Critical> {
    let mut printer_state_text = "Offline";
    let mut printer_state_code = PrintStateCode::Offline;
    let mut gcode_line = 0;
    let mut gcode_pct = 0.;

    enum PrintTime {
        Printing {
            started: Instant,
            remaining: Option<Duration>,
        },
        Done {
            took: Duration,
        },
    }

    let mut time_stats: Option<PrintTime> = None;
    let mut active_gcode: Option<Arc<Gcode>> = None;

    loop {
        target.store(Some(Arc::new(json!({
            "state": printer_state_text,
            "job": {
                "file": match active_gcode {
                    Some(ref gcode) => json!({
                        "date": gcode.meta.modified_time.duration_since(UNIX_EPOCH)?.as_secs(),
                        "display": gcode.meta.source.targeted_filename().file_name(),
                        "name": gcode.meta.source.targeted_filename().file_name(),
                        "origin": "local",
                        "path": gcode.meta.source.targeted_filename().as_str().trim_start_matches('/'),
                        "size": gcode.content.len()
                    }),
                    None => json!({
                        "date": null,
                        "display": null,
                        "name": null,
                        "origin": null,
                        "path": null,
                        "size": null
                    })
                }
            },
            "progress": {
                "completion": gcode_pct,
                "filepos": (gcode_line > 0).then(|| active_gcode.as_ref().and_then(|x| x.line_table.get(gcode_line as usize - 1))),
                "printTime": match time_stats {
                    None => None,
                    Some(PrintTime::Done { took }) => Some(took.as_secs()),
                    Some(PrintTime::Printing { started, .. }) => Some(started.elapsed().as_secs())
                },
                "printTimeLeft": match time_stats {
                    Some(PrintTime::Done { .. }) => Some(0),
                    Some(PrintTime::Printing { remaining: Some(remaining), .. }) => Some(remaining.as_secs()),
                    _ => None
                },
                "printTimeLeftOrigin": match time_stats {
                    Some(PrintTime::Printing { remaining: None, .. }) | None => None,
                    _ => Some("analysis")
                }
            }
        }))));

        tokio::select! {
            evt = event_stream_in.recv() => {
                match evt? {
                    PrinterEvent::PrintState { raw, code } => {
                        printer_state_text = raw;
                        match (printer_state_code, code) {
                            (PrintStateCode::Paused | PrintStateCode::Printing, PrintStateCode::Printing) => {},
                            (_, PrintStateCode::Printing) => {
                                match time_stats {
                                    Some(PrintTime::Printing { ref mut started, .. }) => *started = Instant::now(),
                                    _ => time_stats = Some(PrintTime::Printing { started: Instant::now(), remaining: None })
                                }
                            },
                            (_, PrintStateCode::Failed | PrintStateCode::Idle | PrintStateCode::Finished | PrintStateCode::Cancelled) => {
                                if let Some(PrintTime::Printing { started, .. }) = time_stats { time_stats = Some(PrintTime::Done { took: started.elapsed() }) }
                            },
                            (_, PrintStateCode::Offline) => {
                                time_stats = None;
                            }
                            _ => {},
                        }
                        printer_state_code = code;
                    },
                    PrinterEvent::GcodeLine(line) => gcode_line = line,
                    PrinterEvent::PrintPercentage(pct) => gcode_pct = pct,
                    PrinterEvent::RemainingPrintTime(dur) => {
                        if let Some(PrintTime::Printing { ref mut remaining, .. }) = time_stats {
                            *remaining = Some(dur);
                        }
                    },
                    _ => {},
                }
            },
            evt = gcode_stream_in.changed() => {
                evt?;
                match gcode_stream_in.borrow_and_update().clone() {
                    CurrentGcode::Outdated(x) | CurrentGcode::Current(x) => active_gcode = Some(x),
                    _ => {
                        active_gcode = None;
                        gcode_pct = 0.;
                        gcode_line = 0;
                    }
                };
            }
        }
    }
}

struct ProvidedAuth;

struct ApiState {
    current_state_response: StaticOutput,
    current_job_response: StaticOutput,
    current_gcode_watcher: watch::Receiver<CurrentGcode>,
    cfg: Arc<ServerConfig>,
}

impl ApiState {
    fn new(cfg: Arc<ServerConfig>, gcode_watcher: watch::Receiver<CurrentGcode>) -> Self {
        Self {
            current_state_response: None.into(),
            current_job_response: None.into(),
            current_gcode_watcher: gcode_watcher,
            cfg,
        }
    }
}

impl<T: Deref<Target = ApiState> + Sync> FromRequestParts<T> for ProvidedAuth {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &T,
    ) -> Result<ProvidedAuth, Self::Rejection> {
        if let Some(ref needs_auth) = state.cfg.authorization {
            let token = {
                if let Some(x) = parts.headers.get("X-Api-Key") {
                    x.to_str()
                        .map_err(|_| (StatusCode::BAD_REQUEST, "Malformed API key"))?
                } else if let Some(x) = parts.headers.get("Authorization") {
                    x.to_str()
                        .ok()
                        .and_then(|x| x.strip_prefix("Bearer "))
                        .ok_or((StatusCode::BAD_REQUEST, "Malformed Authorization header"))?
                } else {
                    return Err((StatusCode::UNAUTHORIZED, "Missing API key"));
                }
            };

            if !needs_auth.api_keys.iter().any(|x| x == token) {
                return Err((StatusCode::UNAUTHORIZED, "Invalid API key"));
            }
        }

        Ok(ProvidedAuth)
    }
}

async fn handler_impl(content: Option<Arc<Value>>) -> Result<Json<Arc<Value>>, StatusCode> {
    match content {
        None => Err(StatusCode::NO_CONTENT),
        Some(v) => Ok(Json(v)),
    }
}

fn make_handler(
    field: impl Fn(&ApiState) -> &StaticOutput + Sync + Send + Clone + 'static,
) -> MethodRouter<Arc<ApiState>> {
    get(move |_auth: ProvidedAuth, state: State<Arc<ApiState>>| {
        handler_impl(field(&state).load_full())
    })
}

async fn handle_gcode_download(
    _auth: ProvidedAuth,
    State(state): State<Arc<ApiState>>,
    Path(path): Path<String>,
) -> Response {
    let (is_outdated, gcode) = match &*state.current_gcode_watcher.borrow() {
        CurrentGcode::None => {
            return StatusCode::NOT_FOUND.into_response();
        }
        CurrentGcode::Outdated(arc) => (true, arc.clone()),
        CurrentGcode::Current(arc) => (false, arc.clone()),
    };

    if PathBuf::from(path).normalize().components().ne(gcode
        .meta
        .source
        .targeted_filename()
        .components()
        .filter(|x| !x.is_root()))
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    if is_outdated {
        return (StatusCode::CONFLICT, "Still fetching new gcode").into_response();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(axum::body::Body::from(axum::body::Bytes::from_owner(
            gcode.content.clone(),
        )))
        .unwrap()
}

pub async fn serve_octoprint(
    cfg: Arc<ServerConfig>,
    event_stream_in: broadcast::Receiver<PrinterEvent>,
    gcode_stream_in: watch::Receiver<CurrentGcode>,
) -> anyhow::Result<Critical> {
    let state = Arc::new(ApiState::new(cfg, gcode_stream_in.clone()));

    let api_printer_refresher =
        refresh_api_printer(event_stream_in.resubscribe(), &state.current_state_response);
    let api_job_refresher = refresh_api_job(
        event_stream_in,
        gcode_stream_in,
        &state.current_job_response,
    );

    let router = Router::new()
        .route(
            "/api/printer",
            make_handler(|state| &state.current_state_response),
        )
        .route(
            "/api/job",
            make_handler(|state| &state.current_job_response),
        )
        .route("/downloads/files/local/{*path}", get(handle_gcode_download))
        .with_state(state.clone());

    info!("spinning up on {:?}", (state.cfg.bind, state.cfg.port));

    let webserver = tokio::spawn(
        axum::serve(
            TcpListener::bind((state.cfg.bind, state.cfg.port)).await?,
            router,
        )
        .into_future(),
    );

    tokio::select! {
        Err(e) = api_printer_refresher => Err(e.context("api_printer_refresher")),
        Err(e) = api_job_refresher => Err(e.context("api_job_refresher")),
        err = webserver => {
            err??;
            bail!("webserver died");
        }
    }
}
