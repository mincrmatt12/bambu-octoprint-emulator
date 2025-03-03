use std::{
    borrow::Cow,
    io::Read,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::bail;
use async_std::io::{BufReader, ReadExt};
use derivative::Derivative;
use lazy_regex::{lazy_regex, Lazy};
use regex::Regex;
use suppaftp::{
    async_native_tls::TlsConnector, list, AsyncNativeTlsConnector, AsyncNativeTlsFtpStream,
    FtpError, FtpResult, Status,
};
use thiserror::Error;
use tokio::{
    select,
    sync::watch,
    time::{sleep, Sleep},
};
use tracing::{debug, error, info, warn};
use typed_path::{Utf8UnixPath, Utf8UnixPathBuf};
use zip::result::ZipError;

use crate::{
    bambu_tls::bbl_printer_connector_builder, config::PrinterConfig, printer_spy::FilePath,
    Critical,
};

pub type PathBuf = Utf8UnixPathBuf;
pub type Path = Utf8UnixPath;

async fn connect_to_ftp(cfg: &PrinterConfig) -> FtpResult<AsyncNativeTlsFtpStream> {
    let mut ftp_stream = AsyncNativeTlsFtpStream::connect_secure_implicit(
        (cfg.ip, 990),
        AsyncNativeTlsConnector::from(TlsConnector::from(bbl_printer_connector_builder())),
        "",
    )
    .await?;

    ftp_stream.login("bblp", &cfg.access_code).await?;

    Ok(ftp_stream)
}

#[derive(Debug, PartialEq, Eq)]
pub enum GcodeMetaSource {
    Resolved(FilePath),
    Pending3MF {
        known: PathBuf,
        hint_plate_file: Option<usize>,
    },
}

impl GcodeMetaSource {
    pub fn targeted_filename(&self) -> &Path {
        match self {
            GcodeMetaSource::Resolved(inner) => inner.targeted_filename().expect("GcodeMeta.source == NoJob"),
            GcodeMetaSource::Pending3MF {
                known: sdcard, ..
            } => sdcard,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct GcodeMeta {
    pub source: GcodeMetaSource, // may be distinct from the incoming FilePath (canonicalization of names,
    // for instance)
    pub modified_time: SystemTime,
    pub original_filesize: usize,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Gcode {
    pub meta: GcodeMeta,
    #[derivative(Debug = "ignore")]
    pub line_table: Arc<[usize]>,
    #[derivative(Debug = "ignore")]
    pub content: Arc<[u8]>,
}

impl Gcode {
    pub fn new(meta: GcodeMeta, content: Arc<[u8]>) -> Self {
        let mut line_table = vec![0];

        for (offs, c) in content.iter().enumerate() {
            if *c == '\n' as u8 {
                line_table.push(offs + 1);
            }
        }

        if line_table.last().copied() == Some(content.len()) {
            line_table.pop();
        }

        Self {
            meta,
            line_table: line_table.into(),
            content,
        }
    }
}

async fn move_to_containing_directory(
    conn: &mut AsyncNativeTlsFtpStream,
    file: &Path,
) -> FtpResult<()> {
    assert!(file.has_root() && file.file_name().is_some());
    // change dirs
    conn.cwd(file.parent().unwrap()).await?;

    Ok(())
}

async fn download_file(
    conn: &mut AsyncNativeTlsFtpStream,
    file: &Path,
    expected_size: Option<usize>,
) -> FtpResult<Arc<[u8]>> {
    move_to_containing_directory(conn, file).await?;

    let file_name = file.file_name().unwrap();
    let mut incoming = BufReader::new(conn.retr_as_stream(file_name).await?);
    let mut result = Vec::new();

    if let Some(target) = expected_size {
        result.reserve(target);
    } else if let Ok(target) = conn.size(file_name).await {
        result.reserve(target);
    }

    incoming
        .read_to_end(&mut result)
        .await
        .map_err(|x| FtpError::ConnectionError(x))?;

    Ok(result.into())
}

// if file is absolute, we get the listing containing it
// (note we don't use MLST because the printers don't support it)
//
// otherwise, scan through the configured directories to find it (currently `/` and `/models`)
async fn search_for_matching_file<'a>(
    conn: &mut AsyncNativeTlsFtpStream,
    file: &'a Path,
) -> FtpResult<Option<(Cow<'a, Path>, list::File)>> {
    let dirs_or_none: &'static [Option<&'static str>] = if file.has_root() {
        move_to_containing_directory(conn, file).await?;
        &[None]
    } else {
        &[Some("/"), Some("/model")]
    };

    for possible_dir in dirs_or_none {
        if let Some(target_dir) = possible_dir {
            match conn.cwd(target_dir).await {
                Ok(_) => {}
                Err(FtpError::UnexpectedResponse(resp))
                    if resp.status == Status::FileUnavailable =>
                {
                    continue;
                }
                err @ _ => err?,
            }
        }

        for entry in conn
            .list(None)
            .await?
            .iter()
            .map(|x| list::File::try_from(x.as_str()))
            .filter_map(|x| x.ok())
        {
            if entry.name() == file.file_name().unwrap() {
                if possible_dir.is_none() {
                    return Ok(Some((Cow::Borrowed(file), entry)));
                } else {
                    let mut reconstructed = PathBuf::from(possible_dir.unwrap());
                    reconstructed.push(entry.name());
                    return Ok(Some((Cow::Owned(reconstructed), entry)));
                }
            }
        }
    }

    Ok(None)
}

static CACHE_PLATE_RE: Lazy<Regex> = lazy_regex!(r"^_plate_(\d+).gcode$");

async fn search_for_newest_cache(
    conn: &mut AsyncNativeTlsFtpStream,
    file: &Path,
) -> FtpResult<Option<usize>> {
    match conn.cwd("/cache").await {
        Ok(()) => {}
        Err(FtpError::UnexpectedResponse(resp)) if resp.status == Status::FileUnavailable => {
            return Ok(None);
        }
        err @ _ => err?,
    }

    let expected_prefix = match file.file_name().unwrap().split_once('.') {
        Some((x, _)) => x,
        None => return Ok(None),
    };

    let mut newest_candidate: Option<(SystemTime, usize)> = None;
    for entry in conn
        .list(None)
        .await?
        .iter()
        .map(|x| list::File::try_from(x.as_str()))
        .filter_map(|x| x.ok())
    {
        if let Some(candidate) = entry.name().strip_prefix(expected_prefix) {
            if let Some(matches) = CACHE_PLATE_RE.captures(candidate) {
                if let Some(idx) = matches
                    .get(1)
                    .and_then(|x| x.as_str().parse::<usize>().ok())
                {
                    if newest_candidate.is_none()
                        || newest_candidate.as_ref().unwrap().0 < entry.modified()
                    {
                        newest_candidate = Some((entry.modified(), idx));
                    }
                }
            }
        }
    }

    Ok(newest_candidate.map(|x| x.1))
}

#[derive(Debug, Error)]
enum GcodeError {
    #[error(transparent)]
    Ftp(#[from] FtpError),
    #[error(transparent)]
    Zip(#[from] ZipError),
    #[error("no gcode present in 3MF")]
    NoGcodeIn3MF,
    #[error("file not found")]
    FileNotFound,
}

static PLATE_RE: Lazy<Regex> = lazy_regex!(r"^Metadata/plate_(\d+).gcode$");

impl GcodeMeta {
    fn new(source: GcodeMetaSource, witness: &list::File) -> Self {
        Self {
            source,
            modified_time: witness.modified(),
            original_filesize: witness.size(),
        }
    }

    // only serves to download a fully resolved file
    async fn resolve(
        mut self,
        conn: &mut AsyncNativeTlsFtpStream,
    ) -> Result<Arc<Gcode>, GcodeError> {
        // Obtain the file contents
        let target = self.source.targeted_filename();

        let content_raw = download_file(conn, target, Some(self.original_filesize)).await?;
        let mut zipfile = match self.source {
            GcodeMetaSource::Resolved(FilePath::Inside3MF { .. })
            | GcodeMetaSource::Pending3MF { .. } => {
                zip::ZipArchive::new(std::io::Cursor::new(&content_raw))?
            }
            GcodeMetaSource::Resolved(FilePath::StrippedPath(_)) => {
                return Ok(Arc::new(Gcode::new(self, content_raw)))
            }
            _ => unreachable!(),
        };

        if let GcodeMetaSource::Pending3MF {
            known,
            hint_plate_file,
        } = self.source
        {
            // we don't know which file to use, enumerate all of them
            let mut fallback: Option<PathBuf> = None;
            for (potential, capture) in zipfile.file_names().filter_map(|x| {
                PLATE_RE.captures(x).and_then(|c| {
                    c.get(1)
                        .and_then(|m| m.as_str().parse::<usize>().ok())
                        .map(|y| (x, y))
                })
            }) {
                if fallback.is_none() {
                    fallback = Some(potential.into());
                }
                if let Some(hint) = hint_plate_file {
                    if hint == capture {
                        fallback = Some(potential.into());
                        break;
                    }
                }
            }
            if let Some(fname) = fallback {
                self.source = GcodeMetaSource::Resolved(FilePath::Inside3MF {
                    sdcard: known,
                    subpath: fname,
                });
            } else {
                return Err(GcodeError::NoGcodeIn3MF);
            }
        }
        let target = match self.source {
            GcodeMetaSource::Resolved(FilePath::Inside3MF { ref subpath, .. }) => {
                zipfile.by_name(subpath.as_str())?
            }
            _ => unreachable!(),
        };
        let new_contents = target
            .bytes()
            .collect::<Result<Arc<[u8]>, _>>()
            .map_err(|x| GcodeError::Zip(x.into()))?;
        Ok(Arc::new(Gcode::new(self, new_contents)))
    }

    // work out what file to download
    async fn interrogate(
        target: FilePath,
        conn: &mut AsyncNativeTlsFtpStream,
    ) -> Result<Self, GcodeError> {
        let (mut source, listing) = match target {
            FilePath::Inside3MF { ref sdcard, .. } => {
                if let Some((_, listing)) = search_for_matching_file(conn, sdcard).await? {
                    (GcodeMetaSource::Resolved(target), listing)
                } else {
                    return Err(GcodeError::FileNotFound);
                }
            }
            FilePath::StrippedPath(original_path) => {
                let search_result = search_for_matching_file(conn, &original_path).await?;

                if let Some((full_path, listing)) = search_result {
                    if full_path.extension() == Some("3mf") {
                        (
                            GcodeMetaSource::Pending3MF {
                                known: full_path.into(),
                                hint_plate_file: None,
                            },
                            listing,
                        )
                    } else {
                        (
                            GcodeMetaSource::Resolved(FilePath::StrippedPath(full_path.into())),
                            listing,
                        )
                    }
                } else {
                    return Err(GcodeError::FileNotFound);
                }
            }
            FilePath::NoJob => panic!("GcodeMeta.source == NoJob"),
        };

        if let GcodeMetaSource::Pending3MF {
            ref known,
            ref mut hint_plate_file,
        } = source
        {
            *hint_plate_file = search_for_newest_cache(conn, known).await?;
        }

        Ok(Self::new(source, &listing))
    }
}

#[derive(Debug, Clone, Default)]
pub enum CurrentGcode {
    // NoJob / nothing currently available
    #[default]
    None,
    // Might be invalid (but still keeping around in case we can use it as a cache)
    Outdated(Arc<Gcode>),
    // Valid for the last seen FilePath
    Current(Arc<Gcode>),
}

enum CachedConnection {
    // not connected
    Dark,
    // on a timeout before closing
    Cached {
        conn: AsyncNativeTlsFtpStream,
        inactivity_timer: Pin<Box<Sleep>>,
    },
    // actively being used
    InUse,
}

static CONN_CACHE_DURATION: Duration = Duration::from_secs(15);

struct CachedConnectionRef<'a> {
    upstream: &'a mut CachedConnection,
    conn: ManuallyDrop<AsyncNativeTlsFtpStream>,
}

impl<'a> DerefMut for CachedConnectionRef<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl<'a> Deref for CachedConnectionRef<'a> {
    type Target = AsyncNativeTlsFtpStream;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl CachedConnection {
    fn new() -> CachedConnection {
        CachedConnection::Dark
    }

    async fn reuse_or_connect<'a>(
        &'a mut self,
        cfg: &PrinterConfig,
    ) -> FtpResult<CachedConnectionRef<'a>> {
        let new_conn = match std::mem::replace(self, CachedConnection::InUse) {
            CachedConnection::InUse => panic!("cached connection still borrowed"),
            CachedConnection::Dark => connect_to_ftp(cfg).await?,
            CachedConnection::Cached { conn, .. } => conn,
        };

        Ok(CachedConnectionRef {
            upstream: self,
            conn: ManuallyDrop::new(new_conn),
        })
    }

    // Returns if and only if a previously cached connection expires (for logging purposes).
    // If no connection is active (or if the connection is InUse, which should be impossible)
    // it loops forever.
    async fn process(&mut self) -> () {
        let timer = match self {
            CachedConnection::Cached {
                inactivity_timer, ..
            } => Some(inactivity_timer),
            _ => None,
        };

        if let Some(timer) = timer {
            timer.await;
            *self = CachedConnection::Dark;
        } else {
            loop {
                sleep(Duration::MAX).await;
            }
        }
    }
}

impl<'a> Drop for CachedConnectionRef<'a> {
    fn drop(&mut self) {
        *self.upstream = CachedConnection::Cached {
            // SAFETY: in drop, never used again
            conn: unsafe { ManuallyDrop::take(&mut self.conn) },
            inactivity_timer: Box::pin(sleep(CONN_CACHE_DURATION)),
        };
    }
}

pub async fn slurp_gcode(
    cfg: Arc<PrinterConfig>,
    mut file_stream_in: watch::Receiver<FilePath>,
    gcode_stream_out: watch::Sender<CurrentGcode>,
) -> anyhow::Result<Critical> {
    let mut ftp_conn = CachedConnection::new();

    'main: loop {
        let event = select! {
            changed = file_stream_in.changed() => {
                changed?;
                file_stream_in.borrow_and_update().clone()
            },
            _ = ftp_conn.process() => {
                info!("disconnecting FTP due to inactivity");
                continue;
            }
        };

        let mut cached_old = None;
        match event {
            FilePath::NoJob => {
                gcode_stream_out.send_replace(CurrentGcode::None);
                continue;
            }
            _ => {
                gcode_stream_out.send_if_modified(|x| {
                    if let CurrentGcode::Current(old) = x {
                        // mark the gcode as out-of-date
                        cached_old = Some(old.clone());
                        *x = CurrentGcode::Outdated(old.clone());
                        true
                    } else {
                        false
                    }
                });
            }
        }

        // We have an actual event from the printer, initiate a connection
        let mut conn = 'conn: {
            for tries in 0.. {
                if tries > 3 {
                    warn!("failed to connect to FTP server, ignoring event");
                    continue 'main;
                }

                match ftp_conn.reuse_or_connect(&cfg).await {
                    Ok(x) => break 'conn x,
                    Err(FtpError::ConnectionError(_)) => sleep(Duration::from_secs(2)).await,
                    Err(unrecoverable) => {
                        error!(err = ?unrecoverable, "unable to connect to FTP; dying");
                        bail!(unrecoverable);
                    }
                }
            }
            unreachable!();
        };

        // Work out what file we need to steal
        let targeted_meta = match GcodeMeta::interrogate(event, &mut conn).await {
            Ok(x) => x,
            Err(err) => {
                warn!(%err, "unable to find file on the FTP server, ignoring");
                continue 'main;
            }
        };

        if let Some(old) = cached_old {
            if old.meta == targeted_meta {
                info!(?targeted_meta, "using old cache instead of re-downloading");
                gcode_stream_out.send_replace(CurrentGcode::Current(old));
                continue 'main;
            }
        }

        info!(?targeted_meta, "downloading target file");
        gcode_stream_out.send_replace(CurrentGcode::Current(
            match targeted_meta.resolve(&mut conn).await {
                Ok(x) => x,
                Err(err) => {
                    warn!(%err, "unable to download file from FTP server, ignoring");
                    continue 'main;
                }
            },
        ));
    }
}
