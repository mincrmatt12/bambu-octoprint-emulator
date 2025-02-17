use std::process::exit;

use config::Settings;
use printer_spy::spy_on_printer;
use tokio::sync::{broadcast, watch};
use tracing::{debug, error, info};

mod bambu_tls;
mod config;
mod octoprint_server;
mod printer_spy;
mod slurper;

#[tokio::main]
async fn main() {
    // Setup logging
    tracing::subscriber::set_global_default(tracing_subscriber::FmtSubscriber::new()).unwrap();

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
    let (printer_event_out, mut printer_events) = broadcast::channel(8);
    let (file_stream_out, mut file_stream) = watch::channel(Default::default());

    tokio::spawn(async move {
        loop {
            if let Ok(e) = printer_events.recv().await {
                info!(?e, "got event");
            } else {
                return;
            }
        }
    });

    tokio::spawn(async move {
        loop {
            if let Ok(_) = file_stream.changed().await {
                let res = &*file_stream.borrow_and_update();
                info!(?res, "got file stream");
            } else {
                return;
            }
        }
    });

    spy_on_printer(settings.printer.clone(), printer_event_out, file_stream_out)
        .await
        .unwrap();
}
