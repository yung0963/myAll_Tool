//! Asynchronous FTP and FTPS file operations for Navop.
//!
//! The backend keeps the protocol implementation behind a small trait.
//! `suppaftp` has separate concrete stream types for plain FTP and native-tls
//! FTPS, so [`SuppaFtpClient`] hides that detail from callers.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use suppaftp::Status;
use suppaftp::list::{File as SuppaFile, ListParser, PosixPexQuery};
use suppaftp::tokio::{
    AsyncFtpStream, AsyncNativeTlsConnector, AsyncNativeTlsFtpStream, ImplAsyncFtpStream,
    TokioTlsStream,
};
use suppaftp::types::{FileType, FtpError as SuppaFtpError};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::lookup_host;

/// Transport security used by an FTP connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FtpSecurity {
    /// Unencrypted FTP (usually port 21).
    #[default]
    Plain,
    /// Explicit FTPS: connect to FTP and upgrade with `AUTH TLS`.
    ExplicitTls,
    /// Implicit FTPS: establish TLS before reading the welcome response.
    ImplicitTls,
}

/// Data-channel mode used for transfers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FtpTransferMode {
    /// The server opens a data endpoint and the client connects to it.
    #[default]
    Passive,
    /// The client listens and asks the server to connect back.
    Active,
}

/// Configuration for an FTP or FTPS connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub initial_directory: String,
    pub security: FtpSecurity,
    pub transfer_mode: FtpTransferMode,
    pub accept_invalid_certs: bool,
    pub timeout: Duration,
}

impl Default for FtpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 21,
            username: "anonymous".to_string(),
            password: "anonymous@".to_string(),
            initial_directory: "/".to_string(),
            security: FtpSecurity::Plain,
            transfer_mode: FtpTransferMode::Passive,
            accept_invalid_certs: false,
            timeout: Duration::from_secs(20),
        }
    }
}

impl FtpConfig {
    /// Validate values before attempting network I/O.
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!("FTP host is required");
        }
        if self.host.contains(['\r', '\n', '\0']) {
            bail!("FTP host contains an invalid control character");
        }
        if self.port == 0 {
            bail!("FTP port must be greater than zero");
        }
        if self.username.trim().is_empty() {
            bail!("FTP username is required");
        }
        if self.username.contains(['\r', '\n', '\0']) {
            bail!("FTP username contains an invalid control character");
        }
        if self.password.contains(['\r', '\n', '\0']) {
            bail!("FTP password contains an invalid control character");
        }
        if !self.initial_directory.starts_with('/') {
            bail!("FTP initial directory must be absolute");
        }
        validate_remote_path(&self.initial_directory)?;
        if self.timeout.is_zero() {
            bail!("FTP timeout must be greater than zero");
        }
        Ok(())
    }

    /// Return the host/port pair accepted by Tokio's resolver.
    pub fn endpoint(&self) -> String {
        let host = self.host.trim();
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{}", self.port)
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// A file or directory returned by an FTP listing.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub permissions: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub user: Option<String>,
    pub group: Option<String>,
}

impl FileEntry {
    /// Return a useful owner label when a server exposes a numeric UID.
    pub fn owner_display(&self) -> Option<String> {
        match (&self.user, self.uid) {
            (Some(user), _) if !user.is_empty() => Some(user.clone()),
            (_, Some(uid)) => Some(uid.to_string()),
            _ => None,
        }
    }
}

/// Metadata returned by [`FtpClient::stat`].
#[derive(Debug, Clone)]
pub struct PathMetadata {
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub permissions: u32,
}

/// Progress information shared by single-file and recursive operations.
#[derive(Debug, Clone, Default)]
pub struct TransferProgress {
    pub transferred: u64,
    pub total: u64,
    pub speed: f64,
    pub current_file: Option<String>,
    pub current_file_transferred: u64,
    pub current_file_total: u64,
}

/// Policy used when an upload targets an existing remote directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DirectoryConflictPolicy {
    /// Keep existing remote entries and overwrite files as requested.
    #[default]
    Merge,
    /// Delete the existing remote tree before uploading the new tree.
    Replace,
}

/// Callback invoked during transfers.
pub type ProgressCallback = Box<dyn Fn(TransferProgress) + Send + Sync + 'static>;

/// Error returned when a transfer is cancelled by the caller.
#[derive(Debug, Clone, Copy)]
pub struct TransferCancelled;

impl std::fmt::Display for TransferCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FTP transfer cancelled")
    }
}

impl std::error::Error for TransferCancelled {}

/// Async operations supported by the FTP backend.
#[async_trait]
pub trait FtpClient: Send + Sync {
    async fn connect(config: FtpConfig) -> Result<Self>
    where
        Self: Sized;

    async fn list_dir(&mut self, path: &str) -> Result<Vec<FileEntry>>;
    async fn stat(&mut self, path: &str) -> Result<Option<PathMetadata>>;

    async fn download_with_progress(
        &mut self,
        remote_path: &str,
        local_path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()>;

    async fn upload_with_progress(
        &mut self,
        local_path: &str,
        remote_path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()>;

    async fn delete(&mut self, path: &str, is_dir: bool) -> Result<()>;
    async fn delete_recursive(
        &mut self,
        path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()>;
    async fn mkdir(&mut self, path: &str) -> Result<()>;
    async fn rename(&mut self, old_path: &str, new_path: &str) -> Result<()>;

    /// FTP servers do not have a portable chmod command.
    async fn chmod(&mut self, path: &str, mode: u32) -> Result<()>;

    async fn read_file(&mut self, path: &str, max_bytes: usize) -> Result<Vec<u8>>;
    async fn write_file(&mut self, path: &str, content: &[u8]) -> Result<()>;
    async fn list_dir_recursive(
        &mut self,
        path: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Vec<FileEntry>>;
    async fn download_dir_with_progress(
        &mut self,
        remote_path: &str,
        local_path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()>;
    async fn upload_dir_with_progress(
        &mut self,
        local_path: &str,
        remote_path: &str,
        conflict_policy: DirectoryConflictPolicy,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()>;
    async fn disconnect(&mut self) -> Result<()>;
    async fn realpath(&mut self, path: &str) -> Result<String>;
}

enum FtpStream {
    Plain(AsyncFtpStream),
    Tls(AsyncNativeTlsFtpStream),
}

/// `suppaftp`-backed FTP/FTPS client.
pub struct SuppaFtpClient {
    stream: Option<FtpStream>,
    config: FtpConfig,
}

/// Alias kept for callers that prefer a backend-oriented name.
pub type FtpBackend = SuppaFtpClient;

/// Alias for code that describes the connection rather than its implementation.
pub type FtpConnection = SuppaFtpClient;

impl SuppaFtpClient {
    /// Connect, authenticate, set binary transfer mode, and enter the initial directory.
    pub async fn connect(config: FtpConfig) -> Result<Self> {
        config.validate()?;

        let timeout = config.timeout;
        let host = config.host.trim().to_owned();
        let endpoint = config.endpoint();
        let addresses = timeout_io_result(
            timeout,
            "resolve FTP server",
            lookup_host(endpoint.as_str()),
        )
        .await?
        .collect::<Vec<_>>();
        if addresses.is_empty() {
            bail!("FTP server did not resolve to an address");
        }

        let mut last_error = None;
        for address in addresses {
            match connect_stream(&config, &host, address).await {
                Ok(stream) => {
                    let mut client = Self {
                        stream: Some(stream),
                        config: config.clone(),
                    };
                    // Login/CWD failures are meaningful and should not silently
                    // retry against another address.
                    client.initialize().await?;
                    return Ok(client);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("unable to connect to FTP server")))
    }

    async fn initialize(&mut self) -> Result<()> {
        self.login().await?;
        self.set_binary_mode().await?;
        self.cwd(&self.config.initial_directory.clone()).await?;
        Ok(())
    }

    async fn login(&mut self) -> Result<()> {
        let username = self.config.username.clone();
        let password = self.config.password.clone();
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "FTP login", stream.login(username, password)).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "FTPS login", stream.login(username, password)).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn set_binary_mode(&mut self) -> Result<()> {
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(
                    timeout,
                    "set FTP binary transfer mode",
                    stream.transfer_type(FileType::Binary),
                )
                .await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(
                    timeout,
                    "set FTPS binary transfer mode",
                    stream.transfer_type(FileType::Binary),
                )
                .await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    /// Change the current remote directory.
    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        validate_remote_path(path)?;
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "change FTP directory", stream.cwd(path)).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "change FTPS directory", stream.cwd(path)).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    /// Return the server's current working directory.
    pub async fn pwd(&mut self) -> Result<String> {
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "read FTP working directory", stream.pwd()).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "read FTPS working directory", stream.pwd()).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    /// List a directory.  This is an inherent convenience wrapper around the trait method.
    pub async fn list(&mut self, path: &str) -> Result<Vec<FileEntry>> {
        self.list_dir(path).await
    }

    /// Read a remote file without imposing a caller-specific size limit.
    pub async fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        self.read_file(path, usize::MAX).await
    }

    /// Write or replace a remote file.
    pub async fn write(&mut self, path: &str, content: &[u8]) -> Result<()> {
        self.write_file(path, content).await
    }

    /// Download one file without a progress callback.
    pub async fn download(&mut self, remote_path: &str, local_path: &str) -> Result<()> {
        self.download_with_progress(
            remote_path,
            local_path,
            Arc::new(AtomicBool::new(false)),
            Box::new(|_| {}),
        )
        .await
    }

    /// Upload one file without a progress callback.
    pub async fn upload(&mut self, local_path: &str, remote_path: &str) -> Result<()> {
        self.upload_with_progress(
            local_path,
            remote_path,
            Arc::new(AtomicBool::new(false)),
            Box::new(|_| {}),
        )
        .await
    }

    /// Create a directory and all missing parents.
    pub async fn mkdir_all(&mut self, path: &str) -> Result<()> {
        validate_remote_path(path)?;
        let normalized = path.trim_end_matches('/');
        if normalized.is_empty() || normalized == "/" {
            return Ok(());
        }
        let mut current = String::new();
        for component in normalized.split('/').filter(|part| !part.is_empty()) {
            current.push('/');
            current.push_str(component);
            if let Some(metadata) = self.stat(&current).await? {
                if metadata.is_dir {
                    continue;
                }
                bail!("FTP path exists but is not a directory");
            }
            self.mkdir(&current).await?;
        }
        Ok(())
    }

    async fn list_mlsd(&mut self, path: &str) -> Result<Vec<String>> {
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "list FTP directory", stream.mlsd(Some(path))).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "list FTPS directory", stream.mlsd(Some(path))).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn list_legacy(&mut self, path: &str) -> Result<Vec<String>> {
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "list FTP directory", stream.list(Some(path))).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "list FTPS directory", stream.list(Some(path))).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn list_raw_for_stat(&mut self, path: &str) -> Result<Vec<String>> {
        match self.list_mlsd(path).await {
            Ok(lines) => Ok(lines),
            Err(error) if is_command_unsupported(&error) => self.list_legacy(path).await,
            Err(error) => Err(error),
        }
    }

    async fn mlst(&mut self, path: &str) -> Result<String> {
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "stat FTP path", stream.mlst(Some(path))).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "stat FTPS path", stream.mlst(Some(path))).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn quit_stream(stream: &mut FtpStream, timeout: Duration) -> Result<()> {
        match stream {
            FtpStream::Plain(stream) => {
                timeout_result(timeout, "close FTP connection", stream.quit()).await
            }
            FtpStream::Tls(stream) => {
                timeout_result(timeout, "close FTPS connection", stream.quit()).await
            }
        }
    }
}

#[async_trait]
impl FtpClient for SuppaFtpClient {
    async fn connect(config: FtpConfig) -> Result<Self> {
        Self::connect(config).await
    }

    async fn list_dir(&mut self, path: &str) -> Result<Vec<FileEntry>> {
        validate_remote_path(path)?;
        let lines = match self.list_mlsd(path).await {
            Ok(lines) => lines,
            Err(error) if is_command_unsupported(&error) => self.list_legacy(path).await?,
            Err(error) => return Err(error),
        };
        let mut entries = parse_listing(&lines, path)?;
        entries.sort_by(|left, right| {
            right
                .is_dir
                .cmp(&left.is_dir)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(entries)
    }

    async fn stat(&mut self, path: &str) -> Result<Option<PathMetadata>> {
        validate_remote_path(path)?;
        // MLST gives an unambiguous answer for both files and directories.
        if let Ok(line) = self.mlst(path).await {
            if let Some(file) = parse_mlst_line(&line) {
                return Ok(Some(metadata_from_file(&file)));
            }
        }

        // Older servers do not implement MLST.  CWD is the portable way to
        // distinguish a directory from a missing path in that case.
        let previous = self.pwd().await.ok();
        if self.cwd(path).await.is_ok() {
            if let Some(previous) = previous {
                self.cwd(&previous).await?;
            }
            return Ok(Some(PathMetadata {
                size: 0,
                modified: UNIX_EPOCH,
                is_dir: true,
                permissions: 0o777,
            }));
        }

        match self.list_raw_for_stat(path).await {
            Ok(lines) => {
                if let Some(file) = lines.iter().find_map(|line| parse_listing_line(line)) {
                    return Ok(Some(metadata_from_file(&file)));
                }
                Ok(None)
            }
            Err(error) if is_missing_path(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn download_with_progress(
        &mut self,
        remote_path: &str,
        local_path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()> {
        validate_remote_path(remote_path)?;
        ensure_not_cancelled(&cancelled)?;
        let total = self
            .stat(remote_path)
            .await?
            .ok_or_else(|| anyhow!("remote file is not available"))?;
        if total.is_dir {
            bail!("cannot download a remote directory as a file");
        }

        let destination = PathBuf::from(local_path);
        let temporary = temporary_local_path(&destination);
        if let Some(parent) = temporary.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| "create local download directory")?;
        }
        let result = match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                download_file(
                    stream,
                    remote_path,
                    &temporary,
                    total.size,
                    &cancelled,
                    progress.as_ref(),
                    self.config.timeout,
                )
                .await
            }
            Some(FtpStream::Tls(stream)) => {
                download_file(
                    stream,
                    remote_path,
                    &temporary,
                    total.size,
                    &cancelled,
                    progress.as_ref(),
                    self.config.timeout,
                )
                .await
            }
            None => bail!("FTP connection is closed"),
        };
        match result {
            Ok(transferred) => {
                commit_local_file(&temporary, &destination).await?;
                progress(TransferProgress {
                    transferred,
                    total: total.size,
                    speed: 0.0,
                    current_file: None,
                    current_file_transferred: 0,
                    current_file_total: 0,
                });
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary).await;
                Err(error)
            }
        }
    }

    async fn upload_with_progress(
        &mut self,
        local_path: &str,
        remote_path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()> {
        validate_remote_path(remote_path)?;
        ensure_not_cancelled(&cancelled)?;
        let source = File::open(local_path)
            .await
            .with_context(|| "open local upload file")?;
        let total = source
            .metadata()
            .await
            .with_context(|| "read local upload metadata")?
            .len();
        let name = Path::new(local_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let transferred = match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                upload_file(
                    stream,
                    remote_path,
                    source,
                    total,
                    name,
                    &cancelled,
                    progress.as_ref(),
                    self.config.timeout,
                )
                .await?
            }
            Some(FtpStream::Tls(stream)) => {
                upload_file(
                    stream,
                    remote_path,
                    source,
                    total,
                    name,
                    &cancelled,
                    progress.as_ref(),
                    self.config.timeout,
                )
                .await?
            }
            None => bail!("FTP connection is closed"),
        };
        progress(TransferProgress {
            transferred,
            total,
            speed: 0.0,
            current_file: None,
            current_file_transferred: 0,
            current_file_total: 0,
        });
        Ok(())
    }

    async fn delete(&mut self, path: &str, is_dir: bool) -> Result<()> {
        validate_remote_path(path)?;
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) if is_dir => {
                timeout_result(timeout, "remove FTP directory", stream.rmdir(path)).await
            }
            Some(FtpStream::Tls(stream)) if is_dir => {
                timeout_result(timeout, "remove FTPS directory", stream.rmdir(path)).await
            }
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "remove FTP file", stream.rm(path)).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "remove FTPS file", stream.rm(path)).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn delete_recursive(
        &mut self,
        path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()> {
        validate_remote_path(path)?;
        ensure_not_cancelled(&cancelled)?;
        let Some(root) = self.stat(path).await? else {
            return Ok(());
        };
        if !root.is_dir {
            self.delete(path, false).await?;
            progress(TransferProgress {
                transferred: 1,
                total: 1,
                speed: 0.0,
                current_file: None,
                current_file_transferred: 1,
                current_file_total: 1,
            });
            return Ok(());
        }

        let entries = self.list_dir_recursive(path, cancelled.clone()).await?;
        let total = entries.len() as u64 + 1;
        let mut deleted = 0;
        for entry in entries.iter().filter(|entry| !entry.is_dir) {
            ensure_not_cancelled(&cancelled)?;
            self.delete(&entry.path, false).await?;
            deleted += 1;
            progress(TransferProgress {
                transferred: deleted,
                total,
                speed: 0.0,
                current_file: Some(entry.name.clone()),
                current_file_transferred: 1,
                current_file_total: 1,
            });
        }
        let mut directories: Vec<&FileEntry> =
            entries.iter().filter(|entry| entry.is_dir).collect();
        directories.sort_by_key(|entry| std::cmp::Reverse(remote_path_depth(&entry.path)));
        for entry in directories {
            ensure_not_cancelled(&cancelled)?;
            self.delete(&entry.path, true).await?;
            deleted += 1;
            progress(TransferProgress {
                transferred: deleted,
                total,
                speed: 0.0,
                current_file: Some(entry.name.clone()),
                current_file_transferred: 1,
                current_file_total: 1,
            });
        }
        ensure_not_cancelled(&cancelled)?;
        self.delete(path, true).await?;
        progress(TransferProgress {
            transferred: deleted + 1,
            total,
            speed: 0.0,
            current_file: None,
            current_file_transferred: 0,
            current_file_total: 0,
        });
        Ok(())
    }

    async fn mkdir(&mut self, path: &str) -> Result<()> {
        validate_remote_path(path)?;
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(timeout, "create FTP directory", stream.mkdir(path)).await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(timeout, "create FTPS directory", stream.mkdir(path)).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn rename(&mut self, old_path: &str, new_path: &str) -> Result<()> {
        validate_remote_path(old_path)?;
        validate_remote_path(new_path)?;
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                timeout_result(
                    timeout,
                    "rename FTP path",
                    stream.rename(old_path, new_path),
                )
                .await
            }
            Some(FtpStream::Tls(stream)) => {
                timeout_result(
                    timeout,
                    "rename FTPS path",
                    stream.rename(old_path, new_path),
                )
                .await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn chmod(&mut self, _path: &str, _mode: u32) -> Result<()> {
        bail!("FTP chmod is not portable and is not supported")
    }

    async fn read_file(&mut self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        validate_remote_path(path)?;
        if let Some(metadata) = self.stat(path).await? {
            if metadata.size > max_bytes as u64 {
                bail!("remote file exceeds the configured read limit");
            }
        }
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                read_file_stream(stream, path, max_bytes, timeout).await
            }
            Some(FtpStream::Tls(stream)) => {
                read_file_stream(stream, path, max_bytes, timeout).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn write_file(&mut self, path: &str, content: &[u8]) -> Result<()> {
        validate_remote_path(path)?;
        let timeout = self.config.timeout;
        match self.stream.as_mut() {
            Some(FtpStream::Plain(stream)) => {
                write_bytes_stream(stream, path, content, timeout).await
            }
            Some(FtpStream::Tls(stream)) => {
                write_bytes_stream(stream, path, content, timeout).await
            }
            None => bail!("FTP connection is closed"),
        }
    }

    async fn list_dir_recursive(
        &mut self,
        path: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Vec<FileEntry>> {
        validate_remote_path(path)?;
        let mut pending = vec![path.to_owned()];
        let mut all_entries = Vec::new();
        while let Some(current) = pending.pop() {
            ensure_not_cancelled(&cancelled)?;
            let entries = self.list_dir(&current).await?;
            for entry in entries {
                ensure_not_cancelled(&cancelled)?;
                if entry.is_dir {
                    pending.push(entry.path.clone());
                }
                all_entries.push(entry);
            }
        }
        Ok(all_entries)
    }

    async fn download_dir_with_progress(
        &mut self,
        remote_path: &str,
        local_path: &str,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()> {
        validate_remote_path(remote_path)?;
        ensure_not_cancelled(&cancelled)?;
        let entries = self
            .list_dir_recursive(remote_path, cancelled.clone())
            .await?;
        let total = entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum::<u64>();
        let root = PathBuf::from(local_path);
        fs::create_dir_all(&root)
            .await
            .with_context(|| "create local download root")?;
        let base_remote = remote_path.trim_end_matches('/');
        for entry in entries.iter().filter(|entry| entry.is_dir) {
            ensure_not_cancelled(&cancelled)?;
            let relative = remote_relative_path(base_remote, &entry.path)?;
            fs::create_dir_all(root.join(relative)).await?;
        }

        let mut transferred = 0;
        let started = Instant::now();
        for entry in entries.iter().filter(|entry| !entry.is_dir) {
            ensure_not_cancelled(&cancelled)?;
            let relative = remote_relative_path(base_remote, &entry.path)?;
            let destination = root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).await?;
            }
            let temporary = temporary_local_path(&destination);
            let result = match self.stream.as_mut() {
                Some(FtpStream::Plain(stream)) => {
                    download_file(
                        stream,
                        &entry.path,
                        &temporary,
                        entry.size,
                        &cancelled,
                        progress.as_ref(),
                        self.config.timeout,
                    )
                    .await
                }
                Some(FtpStream::Tls(stream)) => {
                    download_file(
                        stream,
                        &entry.path,
                        &temporary,
                        entry.size,
                        &cancelled,
                        progress.as_ref(),
                        self.config.timeout,
                    )
                    .await
                }
                None => bail!("FTP connection is closed"),
            }?;
            commit_local_file(&temporary, &destination).await?;
            transferred += result;
            progress(TransferProgress {
                transferred,
                total,
                speed: transferred as f64 / started.elapsed().as_secs_f64().max(0.001),
                current_file: Some(entry.name.clone()),
                current_file_transferred: result,
                current_file_total: entry.size,
            });
        }
        progress(TransferProgress {
            transferred,
            total,
            speed: 0.0,
            current_file: None,
            current_file_transferred: 0,
            current_file_total: 0,
        });
        Ok(())
    }

    async fn upload_dir_with_progress(
        &mut self,
        local_path: &str,
        remote_path: &str,
        conflict_policy: DirectoryConflictPolicy,
        cancelled: Arc<AtomicBool>,
        progress: ProgressCallback,
    ) -> Result<()> {
        validate_remote_path(remote_path)?;
        ensure_not_cancelled(&cancelled)?;
        let source_root = PathBuf::from(local_path);
        let metadata = fs::metadata(&source_root)
            .await
            .with_context(|| "read local upload directory")?;
        if !metadata.is_dir() {
            bail!("local upload path is not a directory");
        }

        if conflict_policy == DirectoryConflictPolicy::Replace
            && self.stat(remote_path).await?.is_some()
        {
            self.delete_recursive(remote_path, cancelled.clone(), Box::new(|_| {}))
                .await?;
        }
        self.mkdir_all(remote_path).await?;

        let mut directories = Vec::new();
        let mut files = Vec::new();
        collect_local_tree(&source_root, &mut directories, &mut files).await?;
        directories.sort_by_key(|path| path.components().count());
        files.sort();
        for directory in directories {
            ensure_not_cancelled(&cancelled)?;
            let relative = directory
                .strip_prefix(&source_root)
                .map_err(|_| anyhow!("local upload path is outside its root"))?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            let remote = append_remote_path(remote_path, &relative.to_string_lossy())?;
            self.mkdir_all(&remote).await?;
        }

        let total = files
            .iter()
            .map(|path| std::fs::metadata(path).map(|metadata| metadata.len()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .sum::<u64>();
        let mut transferred = 0;
        let started = Instant::now();
        for file in files {
            ensure_not_cancelled(&cancelled)?;
            let relative = file
                .strip_prefix(&source_root)
                .map_err(|_| anyhow!("local upload path is outside its root"))?;
            let remote = append_remote_path(remote_path, &relative.to_string_lossy())?;
            let size = fs::metadata(&file).await?.len();
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let result = match self.stream.as_mut() {
                Some(FtpStream::Plain(stream)) => {
                    upload_file(
                        stream,
                        &remote,
                        File::open(&file).await?,
                        size,
                        name,
                        &cancelled,
                        progress.as_ref(),
                        self.config.timeout,
                    )
                    .await
                }
                Some(FtpStream::Tls(stream)) => {
                    upload_file(
                        stream,
                        &remote,
                        File::open(&file).await?,
                        size,
                        name,
                        &cancelled,
                        progress.as_ref(),
                        self.config.timeout,
                    )
                    .await
                }
                None => bail!("FTP connection is closed"),
            }?;
            transferred += result;
            progress(TransferProgress {
                transferred,
                total,
                speed: transferred as f64 / started.elapsed().as_secs_f64().max(0.001),
                current_file: file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                current_file_transferred: result,
                current_file_total: size,
            });
        }
        progress(TransferProgress {
            transferred,
            total,
            speed: 0.0,
            current_file: None,
            current_file_transferred: 0,
            current_file_total: 0,
        });
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        let Some(mut stream) = self.stream.take() else {
            return Ok(());
        };
        SuppaFtpClient::quit_stream(&mut stream, self.config.timeout).await
    }

    async fn realpath(&mut self, path: &str) -> Result<String> {
        validate_remote_path(path)?;
        let previous = self.pwd().await?;
        self.cwd(path).await?;
        let result = self.pwd().await;
        self.cwd(&previous).await?;
        result
    }
}

/// Connect to an FTP endpoint using the configured transport.
pub async fn connect(config: FtpConfig) -> Result<SuppaFtpClient> {
    SuppaFtpClient::connect(config).await
}

async fn connect_stream(
    config: &FtpConfig,
    host: &str,
    address: std::net::SocketAddr,
) -> Result<FtpStream> {
    let timeout = config.timeout;
    match config.security {
        FtpSecurity::Plain => {
            let stream = timeout_result(
                timeout,
                "connect to FTP server",
                AsyncFtpStream::connect_timeout(address, timeout),
            )
            .await
            .context("connect to FTP server")?;
            Ok(if config.transfer_mode == FtpTransferMode::Active {
                FtpStream::Plain(stream.active_mode(timeout))
            } else {
                FtpStream::Plain(stream)
            })
        }
        FtpSecurity::ExplicitTls => {
            let stream = timeout_result(
                timeout,
                "connect to explicit FTPS server",
                AsyncNativeTlsFtpStream::connect_timeout(address, timeout),
            )
            .await
            .context("connect to explicit FTPS server")?;
            let connector = tls_connector(config.accept_invalid_certs);
            let stream = timeout_result(
                timeout,
                "upgrade FTP connection to TLS",
                stream.into_secure(connector, host),
            )
            .await
            .context("upgrade FTP connection to TLS")?;
            Ok(if config.transfer_mode == FtpTransferMode::Active {
                FtpStream::Tls(stream.active_mode(timeout))
            } else {
                FtpStream::Tls(stream)
            })
        }
        FtpSecurity::ImplicitTls => {
            let connector = tls_connector(config.accept_invalid_certs);
            let stream = timeout_result(
                timeout,
                "connect to implicit FTPS server",
                AsyncNativeTlsFtpStream::connect_secure_implicit(address, connector, host),
            )
            .await
            .context("connect to implicit FTPS server")?;
            Ok(if config.transfer_mode == FtpTransferMode::Active {
                FtpStream::Tls(stream.active_mode(timeout))
            } else {
                FtpStream::Tls(stream)
            })
        }
    }
}

fn tls_connector(accept_invalid_certs: bool) -> AsyncNativeTlsConnector {
    let connector = suppaftp::async_native_tls::TlsConnector::new()
        .danger_accept_invalid_certs(accept_invalid_certs);
    AsyncNativeTlsConnector::from(connector)
}

async fn timeout_result<T, F>(timeout: Duration, operation: &'static str, future: F) -> Result<T>
where
    F: Future<Output = std::result::Result<T, SuppaFtpError>>,
{
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| anyhow!("{operation} timed out"))?
        .map_err(|error| anyhow::Error::new(error).context(operation))
}

async fn timeout_io_result<T, F>(timeout: Duration, operation: &'static str, future: F) -> Result<T>
where
    F: Future<Output = std::io::Result<T>>,
{
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| anyhow!("{operation} timed out"))?
        .map_err(|error| anyhow!("{operation}: {error}"))
}

fn validate_remote_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("FTP path is required");
    }
    if path.contains(['\r', '\n', '\0']) {
        bail!("FTP path contains an invalid control character");
    }
    Ok(())
}

/// Join a single remote entry name to a directory without allowing a parser
/// to manufacture a path outside that directory.
pub fn join_remote_path(parent: &str, name: &str) -> Result<String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        bail!("invalid FTP entry name");
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok(if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    })
}

fn append_remote_path(parent: &str, child: &str) -> Result<String> {
    let child = child.replace('\\', "/");
    let child = child.trim_matches('/');
    if child.is_empty() {
        return Ok(parent.to_owned());
    }
    validate_remote_path(child)?;
    if parent == "/" {
        Ok(format!("/{child}"))
    } else {
        Ok(format!("{}/{}", parent.trim_end_matches('/'), child))
    }
}

fn parse_listing(lines: &[String], parent: &str) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    let non_empty = lines.iter().filter(|line| !line.trim().is_empty()).count();
    for line in lines {
        if let Some(entry) = parse_listing_line_with_parent(line, parent) {
            entries.push(entry);
        }
    }
    if non_empty > 0 && entries.is_empty() {
        bail!("FTP server returned an unrecognized directory listing");
    }
    Ok(entries)
}

fn parse_listing_line(line: &str) -> Option<SuppaFile> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // `SuppaFile::from_str` intentionally accepts arbitrary text as an MLST
    // filename.  Select a parser from the wire format first so a server error
    // or banner cannot become a phantom file entry.
    if matches!(line.as_bytes().first(), Some(b'-' | b'd' | b'l')) {
        return ListParser::parse_posix(line).ok();
    }
    if line.len() >= 8
        && line.as_bytes().get(2) == Some(&b'-')
        && line.as_bytes().get(5) == Some(&b'-')
    {
        return ListParser::parse_dos(line).ok();
    }
    if line.contains('=') && line.contains(';') {
        return ListParser::parse_mlsd(line).ok();
    }
    None
}

fn parse_listing_line_with_parent(line: &str, parent: &str) -> Option<FileEntry> {
    let file = parse_listing_line(line)?;
    let name = file.name().to_owned();
    let path = join_remote_path(parent, &name).ok()?;
    Some(file_entry_from_file(file, path))
}

fn parse_mlst_line(response: &str) -> Option<SuppaFile> {
    response
        .lines()
        .find(|line| line.contains("type=") && line.contains(';'))
        .and_then(parse_listing_line)
}

fn file_entry_from_file(file: SuppaFile, path: String) -> FileEntry {
    let permissions = permissions_from_file(&file);
    FileEntry {
        name: file.name().to_owned(),
        path,
        size: file.size() as u64,
        modified: file.modified(),
        is_dir: file.is_directory(),
        permissions,
        uid: file.uid(),
        gid: file.gid(),
        user: None,
        group: None,
    }
}

fn metadata_from_file(file: &SuppaFile) -> PathMetadata {
    PathMetadata {
        size: file.size() as u64,
        modified: file.modified(),
        is_dir: file.is_directory(),
        permissions: permissions_from_file(file),
    }
}

fn permissions_from_file(file: &SuppaFile) -> u32 {
    let permission_bits = |query| {
        let mut bits = 0;
        if file.can_read(query) {
            bits |= 4;
        }
        if file.can_write(query) {
            bits |= 2;
        }
        if file.can_execute(query) {
            bits |= 1;
        }
        bits
    };
    (permission_bits(PosixPexQuery::Owner) << 6)
        | (permission_bits(PosixPexQuery::Group) << 3)
        | permission_bits(PosixPexQuery::Others)
}

fn is_missing_path(error: &anyhow::Error) -> bool {
    matches!(
        suppa_status(error),
        Some(Status::FileUnavailable | Status::RequestFileActionIgnored)
    )
}

fn is_command_unsupported(error: &anyhow::Error) -> bool {
    matches!(
        suppa_status(error),
        Some(Status::BadCommand | Status::NotImplemented | Status::NotImplementedParameter)
    )
}

fn suppa_status(error: &anyhow::Error) -> Option<Status> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SuppaFtpError>())
        .and_then(|error| match error {
            SuppaFtpError::UnexpectedResponse(response) => Some(response.status),
            _ => None,
        })
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(TransferCancelled.into());
    }
    Ok(())
}

async fn read_file_stream<T>(
    ftp: &mut ImplAsyncFtpStream<T>,
    path: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>>
where
    T: TokioTlsStream + Send + 'static,
{
    let mut data = ftp
        .retr_as_stream(path)
        .await
        .map_err(|error| anyhow::Error::new(error).context("retrieve FTP file"))?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = match tokio::time::timeout(timeout, data.read(&mut buffer)).await {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => {
                let _ = ftp.abort(data).await;
                return Err(error).context("read FTP data connection");
            }
            Err(_) => {
                let _ = ftp.abort(data).await;
                return Err(anyhow!("read FTP file timed out"));
            }
        };
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > max_bytes {
            let _ = ftp.abort(data).await;
            bail!("remote file exceeds the configured read limit");
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    ftp.finalize_retr_stream(data)
        .await
        .map_err(|error| anyhow::Error::new(error).context("finalize FTP download"))?;
    Ok(bytes)
}

async fn write_bytes_stream<T>(
    ftp: &mut ImplAsyncFtpStream<T>,
    path: &str,
    content: &[u8],
    timeout: Duration,
) -> Result<()>
where
    T: TokioTlsStream + Send + 'static,
{
    let mut data = ftp
        .put_with_stream(path)
        .await
        .map_err(|error| anyhow::Error::new(error).context("open FTP upload"))?;
    match tokio::time::timeout(timeout, data.write_all(content)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = ftp.abort(data).await;
            return Err(error).context("write FTP upload");
        }
        Err(_) => {
            let _ = ftp.abort(data).await;
            return Err(anyhow!("write FTP data timed out"));
        }
    }
    ftp.finalize_put_stream(data)
        .await
        .map_err(|error| anyhow::Error::new(error).context("finalize FTP upload"))
}

async fn download_file<T>(
    ftp: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    destination: &Path,
    expected_size: u64,
    cancelled: &AtomicBool,
    progress: &(dyn Fn(TransferProgress) + Send + Sync),
    timeout: Duration,
) -> Result<u64>
where
    T: TokioTlsStream + Send + 'static,
{
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).await?;
    }
    let file = File::create(destination).await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut data = ftp
        .retr_as_stream(remote_path)
        .await
        .map_err(|error| anyhow::Error::new(error).context("open FTP download"))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut transferred = 0_u64;
    let started = Instant::now();
    loop {
        if let Err(error) = ensure_not_cancelled(cancelled) {
            let _ = ftp.abort(data).await;
            return Err(error);
        }
        let read = match tokio::time::timeout(timeout, data.read(&mut buffer)).await {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => {
                let _ = ftp.abort(data).await;
                return Err(error).context("read FTP download");
            }
            Err(_) => {
                let _ = ftp.abort(data).await;
                return Err(anyhow!("read FTP download timed out"));
            }
        };
        if read == 0 {
            break;
        }
        match tokio::time::timeout(timeout, writer.write_all(&buffer[..read])).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = ftp.abort(data).await;
                return Err(error).context("write local download");
            }
            Err(_) => {
                let _ = ftp.abort(data).await;
                return Err(anyhow!("write local download timed out"));
            }
        }
        transferred += read as u64;
        progress(TransferProgress {
            transferred,
            total: expected_size,
            speed: transferred as f64 / started.elapsed().as_secs_f64().max(0.001),
            current_file: None,
            current_file_transferred: transferred,
            current_file_total: expected_size,
        });
    }
    if transferred != expected_size {
        let _ = ftp.abort(data).await;
        bail!("FTP download size changed while reading");
    }
    ftp.finalize_retr_stream(data)
        .await
        .map_err(|error| anyhow::Error::new(error).context("finalize FTP download"))?;
    writer.flush().await?;
    writer.into_inner().sync_all().await?;
    Ok(transferred)
}

#[allow(clippy::too_many_arguments)]
async fn upload_file<T>(
    ftp: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    source: File,
    expected_size: u64,
    current_file: Option<String>,
    cancelled: &AtomicBool,
    progress: &(dyn Fn(TransferProgress) + Send + Sync),
    timeout: Duration,
) -> Result<u64>
where
    T: TokioTlsStream + Send + 'static,
{
    let mut reader = BufReader::new(source);
    let mut data = ftp
        .put_with_stream(remote_path)
        .await
        .map_err(|error| anyhow::Error::new(error).context("open FTP upload"))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut transferred = 0_u64;
    let started = Instant::now();
    loop {
        if let Err(error) = ensure_not_cancelled(cancelled) {
            let _ = ftp.abort(data).await;
            return Err(error);
        }
        let read = match tokio::time::timeout(timeout, reader.read(&mut buffer)).await {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => {
                let _ = ftp.abort(data).await;
                return Err(error).context("read local upload");
            }
            Err(_) => {
                let _ = ftp.abort(data).await;
                return Err(anyhow!("read local upload timed out"));
            }
        };
        if read == 0 {
            break;
        }
        match tokio::time::timeout(timeout, data.write_all(&buffer[..read])).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = ftp.abort(data).await;
                return Err(error).context("write FTP upload");
            }
            Err(_) => {
                let _ = ftp.abort(data).await;
                return Err(anyhow!("write FTP upload timed out"));
            }
        }
        transferred += read as u64;
        progress(TransferProgress {
            transferred,
            total: expected_size,
            speed: transferred as f64 / started.elapsed().as_secs_f64().max(0.001),
            current_file: current_file.clone(),
            current_file_transferred: transferred,
            current_file_total: expected_size,
        });
    }
    ftp.finalize_put_stream(data)
        .await
        .map_err(|error| anyhow::Error::new(error).context("finalize FTP upload"))?;
    Ok(transferred)
}

fn temporary_local_path(path: &Path) -> PathBuf {
    static NEXT_TEMPORARY_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    path.with_file_name(format!(
        ".{file_name}.navop-partial-{}-{id}",
        std::process::id()
    ))
}

async fn commit_local_file(temporary: &Path, destination: &Path) -> Result<()> {
    match fs::rename(temporary, destination).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(destination).await?;
            fs::rename(temporary, destination).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn collect_local_tree(
    root: &Path,
    directories: &mut Vec<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = entry.metadata().await?;
            if metadata.is_dir() {
                directories.push(path.clone());
                pending.push(path);
            } else if metadata.is_file() {
                files.push(path);
            }
        }
    }
    Ok(())
}

fn remote_relative_path<'a>(base: &str, path: &'a str) -> Result<&'a str> {
    let path = path.strip_prefix(base).unwrap_or(path);
    let path = path.trim_start_matches('/');
    if path.is_empty() || path == "." || path == ".." || path.contains("../") {
        bail!("invalid relative FTP path");
    }
    Ok(path)
}

fn remote_path_depth(path: &str) -> usize {
    path.split('/')
        .filter(|component| !component.is_empty())
        .count()
}

/// Sum the sizes of all non-directory entries in a recursive listing.
pub fn total_file_size(entries: &[FileEntry]) -> u64 {
    entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.size)
        .sum()
}

/// Calculate a remote directory's total file size.
pub async fn calculate_directory_size<C>(
    client: &mut C,
    path: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<u64>
where
    C: FtpClient + ?Sized,
{
    ensure_not_cancelled(&cancelled)?;
    let entries = client.list_dir_recursive(path, cancelled).await?;
    Ok(total_file_size(&entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_uses_safe_interoperable_values() {
        let config = FtpConfig::default();
        assert_eq!(21, config.port);
        assert_eq!(FtpSecurity::Plain, config.security);
        assert_eq!(FtpTransferMode::Passive, config.transfer_mode);
        assert!(!config.accept_invalid_certs);
    }

    #[test]
    fn validation_rejects_missing_endpoint_and_relative_directory() {
        let mut config = FtpConfig::default();
        assert!(config.validate().is_err());
        config.host = "ftp.example.test".to_string();
        config.initial_directory = "uploads".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_rejects_zero_timeout_and_control_characters() {
        let mut config = FtpConfig {
            host: "ftp.example.test".to_string(),
            ..FtpConfig::default()
        };
        config.timeout = Duration::ZERO;
        assert!(config.validate().is_err());
        config.timeout = Duration::from_secs(1);
        config.username.push('\n');
        assert!(config.validate().is_err());
    }

    #[test]
    fn endpoint_brackets_ipv6_literals() {
        let config = FtpConfig {
            host: "2001:db8::1".to_string(),
            ..FtpConfig::default()
        };
        assert_eq!("[2001:db8::1]:21", config.endpoint());
    }

    #[test]
    fn remote_path_join_rejects_traversal() {
        assert_eq!(
            "/pub/file.txt",
            join_remote_path("/pub", "file.txt").unwrap()
        );
        assert!(join_remote_path("/pub", "../secret").is_err());
        assert!(join_remote_path("/pub", "a/b").is_err());
    }

    #[test]
    fn parses_mlsd_and_posix_entries() {
        let lines = vec![
            "type=file;size=12;modify=20240102030405;UNIX.mode=0644; report.txt".to_string(),
            "drwxr-xr-x 1 1000 1000 4096 Jan 2 2024 docs".to_string(),
        ];
        let entries = parse_listing(&lines, "/pub").unwrap();
        assert_eq!(2, entries.len());
        assert!(entries.iter().any(|entry| entry.path == "/pub/report.txt"));
        assert!(
            entries
                .iter()
                .any(|entry| entry.is_dir && entry.name == "docs")
        );
        let report = entries
            .iter()
            .find(|entry| entry.name == "report.txt")
            .unwrap();
        assert_eq!(0o644, report.permissions);
    }

    #[test]
    fn rejects_unrecognized_non_empty_listing() {
        let lines = vec!["not a server listing".to_string()];
        assert!(parse_listing(&lines, "/").is_err());
    }

    #[test]
    fn total_file_size_ignores_directories() {
        let entries = vec![
            FileEntry {
                name: "a".to_string(),
                path: "/a".to_string(),
                size: 10,
                modified: UNIX_EPOCH,
                is_dir: false,
                permissions: 0,
                uid: None,
                gid: None,
                user: None,
                group: None,
            },
            FileEntry {
                name: "dir".to_string(),
                path: "/dir".to_string(),
                size: 100,
                modified: UNIX_EPOCH,
                is_dir: true,
                permissions: 0,
                uid: None,
                gid: None,
                user: None,
                group: None,
            },
        ];
        assert_eq!(10, total_file_size(&entries));
    }
}
