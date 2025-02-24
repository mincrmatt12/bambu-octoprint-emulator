use std::{sync::Arc, time::Duration};

use crate::{slurper::PathBuf, Critical};
use anyhow::bail;
use enum_map::Enum;
use merge::Merge;
use rumqttc::{
    AsyncClient, ConnectionError, Event, EventLoop, MqttOptions, Packet, Publish, QoS, StateError,
};
use tokio::{
    select,
    sync::{broadcast, mpsc, watch},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{bambu_tls::bbl_printer_connector_builder, config::PrinterConfig};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum FilePath {
    /// We heard a project_file command response which pointed at this path on the sdcard
    /// and with this specific subpath.
    Inside3MF { sdcard: PathBuf, subpath: PathBuf },
    /// We've only seen a print status report which reported this as the active gcode_file.
    /// The actual path on the sdcard might be in a subfolder (some recursive searching can be
    /// performed; config allows listing which folders to enumerate in) but more annoyingly
    /// it might be a 3MF where we don't know which plate is being printed.
    StrippedPath(PathBuf),
    /// We don't have an active job. Downstream code usually ignores this and retains the
    /// last known job until receiving an Idle/Offline state.
    #[default]
    NoJob,
}

#[derive(Debug, Clone, Copy, Enum)]
pub enum Thermistor {
    Bed,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintStateCode {
    Printing,
    Paused,
    Idle,
    Finished,
    Failed,
    Cancelled,
    Offline,
}

#[derive(Debug, Clone)]
pub enum PrinterEvent {
    Temperature {
        which: Thermistor,
        value: f64,
    },
    TemperatureTarget {
        which: Thermistor,
        value: f64,
    },
    PrintState {
        raw: &'static str,
        code: PrintStateCode,
    },
    GcodeLine(u64),
}

async fn wait_for_printer(cfg: &PrinterConfig) -> Result<(), anyhow::Error> {
    let client = surge_ping::Client::new(&Default::default())?;
    let mut pinger = client.pinger(cfg.ip, surge_ping::PingIdentifier(1)).await;
    let mut idx: u16 = 0;

    const PAYLOAD: &'static [u8] = &[0; 56];

    loop {
        use surge_ping::SurgeError as E;

        if idx == 1 {
            info!("printer not responding to ping; waiting until it comes back up");
        }

        match pinger.ping(surge_ping::PingSequence(idx), PAYLOAD).await {
            Ok(_) => return Ok(()),
            Err(E::NetworkError | E::Timeout { .. } | E::IOError(_)) => {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => bail!(e),
        }

        idx = idx.wrapping_add(1);
    }
}

fn make_mqtt_client(cfg: &PrinterConfig) -> anyhow::Result<(AsyncClient, EventLoop)> {
    let mut mqttoptions = MqttOptions::new("boe", cfg.ip.to_string(), 8883);
    mqttoptions.set_transport(rumqttc::Transport::tls_with_config(
        bbl_printer_connector_builder().build()?.into(),
    ));
    mqttoptions.set_credentials("bblp", cfg.access_code.clone());
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    Ok(AsyncClient::new(mqttoptions, 32))
}

async fn request_pushall(client: &AsyncClient, cfg: &PrinterConfig) {
    static MSG: &'static [u8] =
        br#"{"pushing":{"command":"pushall","push_target":1,"sequence_id":"20001","version":1}}"#;

    client
        .publish(
            format!("device/{}/request", cfg.serial),
            QoS::AtMostOnce,
            false,
            MSG,
        )
        .await
        .unwrap();
}

enum RawReportKind {
    PushStatus,
    ProjectFile,
}

fn parse_report(report: &serde_json::Value) -> Option<(RawReportKind, &serde_json::Value)> {
    let print = report.get("print")?;

    let report_kind = match print.get("command") {
        None => {
            warn!("got print report without command, ignoring");
            return None;
        }
        Some(serde_json::Value::String(cmd)) => match cmd.as_str() {
            "push_status" => RawReportKind::PushStatus,
            "project_file" => RawReportKind::ProjectFile,
            _ => return None,
        },
        Some(_) => {
            warn!("got print report with non-string command, ignoring");
            return None;
        }
    };

    Some((report_kind, print))
}

fn parse_thermistors_from_push(
    print: &serde_json::Value,
) -> impl Iterator<Item = PrinterEvent> + use<'_> {
    static THERMISTORS: &[(&str, &str, Thermistor)] = &[
        ("nozzle_temper", "nozzle_target_temper", Thermistor::Tool),
        ("bed_temper", "bed_target_temper", Thermistor::Bed),
    ];

    THERMISTORS
        .iter()
        .flat_map(|(cur, tar, tag)| {
            [
                print
                    .get(cur)
                    .and_then(serde_json::Value::as_f64)
                    .map(|temp| PrinterEvent::Temperature {
                        which: *tag,
                        value: temp,
                    }),
                print
                    .get(tar)
                    .and_then(serde_json::Value::as_f64)
                    .map(|temp| PrinterEvent::TemperatureTarget {
                        which: *tag,
                        value: temp,
                    }),
            ]
        })
        .flatten()
}

fn merge_always<T>(left: &mut Option<T>, right: Option<T>) {
    if right.is_some() {
        *left = right;
    }
}

#[derive(Merge, Default, Debug)]
struct PrinterStateInfo {
    #[merge(strategy = merge_always)]
    stg_cur: Option<i64>,
    #[merge(strategy = merge_always)]
    gcode_state: Option<PrintStateCode>,
    #[merge(strategy = merge_always)]
    print_error: Option<u64>,
}

impl PrinterStateInfo {
    fn is_empty(&self) -> bool {
        self.stg_cur.is_none() && self.gcode_state.is_none() && self.print_error.is_none()
    }

    fn is_full(&self) -> bool {
        self.stg_cur.is_some() && self.gcode_state.is_some() && self.print_error.is_some()
    }

    fn as_state_update(&self) -> PrinterEvent {
        assert!(self.is_full());

        let next_psc = match self.gcode_state.unwrap() {
            PrintStateCode::Failed => match self.print_error.unwrap() {
                0x0500400E | 0x0300400C => PrintStateCode::Cancelled,
                _ => PrintStateCode::Failed,
            },
            pass => pass,
        };

        let next_desc = match next_psc {
            PrintStateCode::Printing => match self.stg_cur.unwrap() {
                2 | 7 => "Preheating",
                14 => "Cleaning nozzle",
                1 | 13 => "Homing",
                4 | 22 | 24 => "Changing filament",
                _ => "Printing",
            },
            PrintStateCode::Cancelled => "Cancelled",
            PrintStateCode::Paused => "Paused",
            PrintStateCode::Idle => "Online",
            PrintStateCode::Failed => "Failed",
            PrintStateCode::Finished => "Finished",
            _ => unreachable!(),
        };

        PrinterEvent::PrintState {
            raw: next_desc,
            code: next_psc,
        }
    }
}

fn parse_printer_state_info(print: &serde_json::Value) -> PrinterStateInfo {
    PrinterStateInfo {
        stg_cur: print.get("stg_cur").and_then(serde_json::Value::as_i64),
        gcode_state: print
            .get("gcode_state")
            .and_then(serde_json::Value::as_str)
            .map(|state| match state {
                "RUNNING" | "PREPARE" => PrintStateCode::Printing,
                "FINISH" => PrintStateCode::Finished,
                "FAILED" => PrintStateCode::Failed,
                "PAUSE" => PrintStateCode::Paused,
                _ => PrintStateCode::Idle,
            }),
        print_error: print.get("print_error").and_then(serde_json::Value::as_u64),
    }
}

fn parse_print_url(url: &str) -> PathBuf {
    static SD_PREFIXES: &[&str] = &["ftp://", "file:///mnt/sdcard", "file:///sdcard"];

    for pfx in SD_PREFIXES {
        if let Some(total) = url.strip_prefix(pfx) {
            return PathBuf::from(total);
        }
    }

    error!(
        url,
        "unable to determine sdcard prefix, just using entire path"
    );
    PathBuf::from(url)
}

async fn listen_from_mqtt(
    mut report_stream: mpsc::UnboundedReceiver<serde_json::Value>,
    event_stream_out: broadcast::Sender<PrinterEvent>,
    file_stream_out: watch::Sender<FilePath>,
    ct: CancellationToken,
) {
    // Retained state objects (merged as partial reports come in)
    let mut state_info: PrinterStateInfo = Default::default();

    // If Some, we've received an IDLE state at some point and thus will begin listening for
    // changes to the gcode_file ahead of project_file commands.
    let mut gcode_file_armed = false;

    loop {
        let report = select! {
            _ = ct.cancelled() => { return; }
            Some(report) = report_stream.recv() => report
        };

        match parse_report(&report) {
            None => continue,
            Some((RawReportKind::PushStatus, print)) => {
                // Handle thermistors
                for therm_event in parse_thermistors_from_push(print) {
                    let _ = event_stream_out.send(therm_event);
                }
                // Decide on the current printer state
                let mut next_state = parse_printer_state_info(print);
                if !next_state.is_empty() {
                    match next_state.gcode_state {
                        Some(
                            PrintStateCode::Finished
                            | PrintStateCode::Cancelled
                            | PrintStateCode::Idle
                            | PrintStateCode::Failed,
                        ) => {
                            gcode_file_armed = true;
                        }
                        _ => {}
                    }
                    // print_error resets to 0 very quickly after a print error; if the print is
                    // still marked failed, we want to retain a non-zero error if possible
                    if next_state.print_error == Some(0)
                        && state_info.gcode_state == Some(PrintStateCode::Failed)
                    {
                        next_state.print_error = None;
                    }
                    // similarly, if we're currently idle or printing, clear out the error
                    match (next_state.gcode_state, next_state.print_error) {
                        (Some(PrintStateCode::Idle | PrintStateCode::Printing), None) => {
                            next_state.print_error = Some(0);
                        }
                        _ => {}
                    }
                    state_info.merge(next_state);
                    if state_info.is_full() {
                        let _ = event_stream_out.send(state_info.as_state_update());
                    }
                }
                // Handle updates to gcode_file to guess current job info
                // (trying not to override data from project_file)
                if let Some(gcode_path) = print
                    .get("gcode_file")
                    .and_then(serde_json::Value::as_str)
                    .map(|v| {
                        if v.is_empty() {
                            None
                        } else {
                            Some(PathBuf::from(v))
                        }
                    })
                {
                    file_stream_out.send_if_modified(|cur| {
                        let modified = match (&cur, &gcode_path) {
                            (FilePath::Inside3MF { sdcard, .. }, Some(new_path))
                                if gcode_file_armed =>
                            {
                                if sdcard.file_name() == new_path.file_name() {
                                    false
                                } else {
                                    gcode_file_armed = false;
                                    true
                                }
                            }
                            (FilePath::NoJob, Some(_)) => true,
                            (FilePath::StrippedPath(current), Some(new)) => current != new,
                            (FilePath::Inside3MF { .. } | FilePath::StrippedPath(_), None) => true,
                            _ => false,
                        };
                        if modified {
                            if let Some(gcode_path) = gcode_path {
                                *cur = FilePath::StrippedPath(gcode_path);
                            } else {
                                *cur = FilePath::NoJob;
                            }
                        }
                        modified
                    });
                }
                // Handle updates to mc_print_line_number
                if let Some(line_number) = print.get("mc_print_line_number").and_then(|x| match x {
                    serde_json::Value::Number(number) => number.as_u64(),
                    serde_json::Value::String(str) => str.parse().ok(),
                    _ => None,
                }) {
                    let _ = event_stream_out.send(PrinterEvent::GcodeLine(line_number));
                }
            }
            Some((RawReportKind::ProjectFile, print)) => {
                let status = print.get("result").and_then(serde_json::Value::as_str);
                if status.is_none() || !status.unwrap().eq_ignore_ascii_case("success") {
                    warn!(%print, "observed failed project_file command");
                    continue;
                }

                let url = print.get("url").and_then(serde_json::Value::as_str);
                let param = print.get("param").and_then(serde_json::Value::as_str);

                if url.is_none() || param.is_none() {
                    warn!(%print, "observed malformed project_file command");
                    continue;
                }

                let url = url.unwrap();
                let param = param.unwrap();

                file_stream_out.send_replace(FilePath::Inside3MF {
                    sdcard: parse_print_url(url),
                    subpath: param.into(),
                });
                gcode_file_armed = false;
            }
        }
    }
}

pub async fn spy_on_printer(
    cfg: Arc<PrinterConfig>,
    event_stream_out: broadcast::Sender<PrinterEvent>,
    file_stream_out: watch::Sender<FilePath>,
) -> anyhow::Result<Critical> {
    loop {
        // Reset file_stream_out to "not printing"
        file_stream_out.send_replace(Default::default());
        // Update current printer status to "offline"
        event_stream_out.send(PrinterEvent::PrintState {
            raw: "Offline",
            code: PrintStateCode::Offline,
        })?;

        // Wait for printer to come up - returns when online and only bails
        // if something is very wrong with the network config
        wait_for_printer(&cfg).await?;
        let (client, mut event_loop) = make_mqtt_client(&cfg)?;
        let (sub_tx, sub_rx) = mpsc::unbounded_channel();
        let sub_ct = CancellationToken::new();
        let sub_task = tokio::spawn({
            let local_es = event_stream_out.clone();
            let local_fs = file_stream_out.clone();
            let sub_ct = sub_ct.clone();
            let local_cfg = cfg.clone();
            async move {
                client
                    .subscribe(
                        format!("device/{}/report", local_cfg.serial),
                        QoS::AtMostOnce,
                    )
                    .await
                    .unwrap();
                request_pushall(&client, &local_cfg).await;
                listen_from_mqtt(sub_rx, local_es, local_fs, sub_ct).await;
            }
        });
        let sub_ct_guard = sub_ct.drop_guard();

        // Run the main MQTT loop
        'conn: loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck { .. })) => {
                    info!("connected to printer MQTT");
                }
                Ok(Event::Incoming(Packet::Publish(Publish { topic, payload, .. })))
                    if topic.ends_with("/report") =>
                {
                    match serde_json::from_slice::<serde_json::Value>(&payload) {
                        Ok(jv) => {
                            sub_tx.send(jv)?;
                        }
                        Err(e) => {
                            warn!(err = %e, "received malformed JSON on /report");
                        }
                    }
                }
                Ok(_) => {}
                Err(ConnectionError::Io(err) | ConnectionError::MqttState(StateError::Io(err))) => {
                    warn!(%err, "lost connection; IO error");
                    break 'conn;
                }
                Err(ConnectionError::MqttState(StateError::ConnectionAborted)) => {
                    warn!("lost connection; aborted");
                    break 'conn;
                }
                Err(ConnectionError::Tls(err)) => {
                    error!(%err, "got TLS error, aborting");
                    bail!(err);
                }
                Err(ConnectionError::ConnectionRefused(code)) => {
                    error!(?code, "printer refused connection, aborting");
                    bail!("printer refused MQTT connection");
                }
                Err(
                    ConnectionError::NetworkTimeout
                    | ConnectionError::FlushTimeout
                    | ConnectionError::MqttState(StateError::AwaitPingResp),
                ) => {
                    warn!("lost connection; timeout");
                    break 'conn;
                }
                Err(err @ (ConnectionError::NotConnAck(_) | ConnectionError::MqttState(_))) => {
                    error!(?err, "printer sent invalid packets, reconnecting");
                    break 'conn;
                }
                Err(err) => {
                    warn!(?err, "unknown error, ignoring");
                }
            }
        }

        // Cancel the underlying task
        drop(sub_ct_guard);
        let _ = sub_task.await;
    }
}
