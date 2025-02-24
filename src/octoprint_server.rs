use std::{borrow::Borrow, future::IntoFuture, ops::Deref, sync::Arc};

use anyhow::bail;
use arc_swap::ArcSwapOption;
use axum::{
    extract::{FromRequestParts, State},
    handler::Handler,
    http::{request::Parts, StatusCode},
    routing::{get, MethodRouter},
    Json, Router,
};
use enum_map::EnumMap;
use serde_json::{json, Map, Value};
use tokio::{
    net::TcpListener,
    sync::{broadcast, watch},
};
use tracing::info;

use crate::{
    config::ServerConfig,
    printer_spy::{FilePath, PrintStateCode, PrinterEvent, Thermistor},
    slurper::CurrentGcode,
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
    }
}

struct ProvidedAuth();

struct ApiState {
    current_state_response: StaticOutput,
    cfg: Arc<ServerConfig>,
}

impl ApiState {
    fn new(cfg: Arc<ServerConfig>) -> Self {
        Self {
            current_state_response: None.into(),
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

        Ok(ProvidedAuth())
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

pub async fn serve_octoprint(
    cfg: Arc<ServerConfig>,
    mut event_stream_in: broadcast::Receiver<PrinterEvent>,
    file_stream_in: watch::Receiver<FilePath>,
    gcode_stream_in: watch::Receiver<CurrentGcode>,
) -> anyhow::Result<Critical> {
    let state = Arc::new(ApiState::new(cfg));

    let api_printer_refresher =
        refresh_api_printer(event_stream_in.resubscribe(), &state.current_state_response);

    let router = Router::new()
        .route(
            "/api/printer",
            make_handler(|state| &state.current_state_response),
        )
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
        err = webserver => {
            err??;
            bail!("webserver died");
        }
    }
}
