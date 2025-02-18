use std::{collections::{BTreeSet, HashMap}, sync::RwLock};

use suppaftp::{async_native_tls::TlsConnector, AsyncNativeTlsConnector, AsyncNativeTlsFtpStream, FtpResult};
use typed_path::UnixPathBuf;

use crate::{bambu_tls::bbl_printer_connector_builder, config::PrinterConfig};

pub type PathBuf = UnixPathBuf;

async fn connect_to_ftp(
    cfg: &PrinterConfig
) -> FtpResult<AsyncNativeTlsFtpStream> {
    let mut ftp_stream = AsyncNativeTlsFtpStream::connect_secure_implicit(
        (cfg.ip, 990),
        AsyncNativeTlsConnector::from(TlsConnector::from(bbl_printer_connector_builder())),
        ""
    ).await?;

    ftp_stream.login("bblp", &cfg.access_code).await?;

    Ok(ftp_stream)
}

pub struct CurrentGcode {
}
