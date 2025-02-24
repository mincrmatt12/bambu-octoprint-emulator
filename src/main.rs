use std::{panic, process::exit};

use config::Settings;
use octoprint_server::serve_octoprint;
use printer_spy::spy_on_printer;
use slurper::slurp_gcode;
use tokio::{
    sync::{broadcast, watch},
    task::JoinSet,
};
use tracing::{debug, error, info, Level};

mod bambu_tls;
mod config;
mod octoprint_server;
mod printer_spy;
mod slurper;

pub enum Critical {}

#[tokio::main]
async fn main() {
    // Setup logging
    tracing_subscriber::fmt()
        //.with_max_level(Level::DEBUG)
        .init();

    info!("starting up");

    // Load settings
    let settings = match Settings::new() {
        Ok(x) => x,
        Err(err) => {
            error!(%err, "failed to load config");
            exit(1);
        }
    };

    // Start main printer spy task
    let (printer_event_out, printer_events) = broadcast::channel(8);
    let (file_stream_out, file_stream) = watch::channel(Default::default());
    let (gcode_stream_out, gcode_stream) = watch::channel(Default::default());

    // Setup task tracker
    let mut critical_tasks = JoinSet::new();

    let spy_task = spy_on_printer(settings.printer.clone(), printer_event_out, file_stream_out);
    let slurp_task = slurp_gcode(
        settings.printer.clone(),
        file_stream.clone(),
        gcode_stream_out,
    );
    let webserver_task = serve_octoprint(
        settings.server.clone(),
        printer_events,
        file_stream.clone(),
        gcode_stream,
    );

    critical_tasks.spawn(slurp_task);
    critical_tasks.spawn(spy_task);
    critical_tasks.spawn(webserver_task);

    if let Some(e) = critical_tasks.join_next().await {
        match e {
            Ok(Err(err)) => {
                error!(%err, "critical task failed");
                exit(1);
            }
            Err(err) => {
                if err.is_panic() {
                    panic::resume_unwind(err.into_panic());
                }
                error!(%err, "join_next failed");
                exit(1);
            }
        }
    }
}
