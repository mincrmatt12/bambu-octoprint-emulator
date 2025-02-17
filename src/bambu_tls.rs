use lazy_static::lazy_static;
use native_tls::{Certificate, TlsConnector, TlsConnectorBuilder};

static BBL_CA_PEM: &'static [u8] = include_bytes!("../res/bbl.pem");

lazy_static! {
    static ref BBL_CA: Certificate = Certificate::from_pem(BBL_CA_PEM).unwrap();
}

pub fn bbl_printer_connector_builder() -> TlsConnectorBuilder {
    let mut builder = TlsConnector::builder();
    builder
        .disable_built_in_roots(true)
        // since we have to use native-tls (because rustls can't handle v1 x509 certificates) we
        // can't do better than accepting all signed certs (in theory this should check Common name
        // == serial)
        .danger_accept_invalid_hostnames(true)
        .add_root_certificate(BBL_CA.clone())
        .use_sni(false);
    builder
}
