use askama::Template;
use axum::{
    extract::{Form, Path as AxumPath, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    net::TcpListener,
    process::Command,
    sync::RwLock,
};
use tower_http::services::ServeDir;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use urlencoding::encode;
use uuid::Uuid;
use walkdir::WalkDir;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Arc::new(AppConfig::from_env()?);
    config.prepare_dirs().await?;

    let state = AppState {
        config,
        jobs: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(home))
        .route("/download", post(start_download))
        .route("/jobs", get(list_jobs))
        .route("/jobs/:job_id", get(job_detail))
        .route("/jobs/:job_id/log", get(job_log))
        .route("/albums/:album", get(album_detail))
        .route("/albums/:album/files/delete", post(delete_file))
        .route("/albums/:album/delete", post(delete_album))
        .route("/archives/:name/edit", get(edit_archive).post(save_archive))
        .route("/settings", get(show_settings).post(update_settings))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state.clone());

    let addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8090".to_string())
        .parse()?;
    info!("Starting server on {}", addr);
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind to {}: {}", addr, e);
            return Err(AppError::Other(format!("Bind failed: {}", e)));
        }
    };

    axum::serve(listener, app)
        .await
        .map_err(|err| AppError::Other(err.to_string()))
}

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    jobs: JobStore,
}

type JobStore = Arc<RwLock<HashMap<Uuid, Arc<RwLock<JobEntry>>>>>;

#[derive(Clone)]
struct AppConfig {
    data_dir: PathBuf,
    yt_dlp_path: PathBuf,
    directories: Arc<RwLock<DirectoryConfig>>,
    settings_path: PathBuf,
}

impl AppConfig {
    fn from_env() -> Result<Self, AppError> {
        let data_dir = std::env::var("DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./data"));
        let yt_dlp_path = std::env::var("YT_DLP_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("yt-dlp"));
        let settings_path = data_dir.join("settings.json");
        let downloads_override = std::env::var("DOWNLOADS_DIR").ok().map(PathBuf::from);
        let archives_override = std::env::var("ARCHIVES_DIR").ok().map(PathBuf::from);
        let default_dirs = DirectoryConfig {
            downloads_dir: data_dir.join("downloads"),
            archives_dir: data_dir.join("archives"),
        };
        let mut directories = if let Ok(contents) = std::fs::read_to_string(&settings_path) {
            serde_json::from_str(&contents).unwrap_or(default_dirs.clone())
        } else {
            default_dirs
        };
        if let Some(custom) = downloads_override {
            directories.downloads_dir = custom;
        }
        if let Some(custom) = archives_override {
            directories.archives_dir = custom;
        }
        Ok(Self {
            data_dir,
            yt_dlp_path,
            settings_path,
            directories: Arc::new(RwLock::new(directories)),
        })
    }

    async fn prepare_dirs(&self) -> Result<(), AppError> {
        fs::create_dir_all(&self.data_dir).await?;
        let dirs = self.directories.read().await.clone();
        fs::create_dir_all(&dirs.downloads_dir).await?;
        fs::create_dir_all(&dirs.archives_dir).await?;
        Ok(())
    }

    async fn directories_snapshot(&self) -> DirectoryConfig {
        self.directories.read().await.clone()
    }

    async fn set_directories(
        &self,
        downloads_dir: PathBuf,
        archives_dir: PathBuf,
    ) -> Result<(), AppError> {
        if downloads_dir.as_os_str().is_empty() || archives_dir.as_os_str().is_empty() {
            return Err(AppError::Invalid(
                "Directory paths must not be empty".to_string(),
            ));
        }
        fs::create_dir_all(&downloads_dir).await?;
        fs::create_dir_all(&archives_dir).await?;
        {
            let mut guard = self.directories.write().await;
            guard.downloads_dir = downloads_dir;
            guard.archives_dir = archives_dir;
        }
        self.persist_directories().await?;
        Ok(())
    }

    async fn downloads_dir(&self) -> PathBuf {
        self.directories.read().await.downloads_dir.clone()
    }

    async fn archives_dir(&self) -> PathBuf {
        self.directories.read().await.archives_dir.clone()
    }

    async fn persist_directories(&self) -> Result<(), AppError> {
        let dirs = self.directories.read().await.clone();
        let serialized = serde_json::to_string_pretty(&dirs)?;
        fs::create_dir_all(&self.data_dir).await?;
        fs::write(&self.settings_path, serialized).await?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct DirectoryConfig {
    downloads_dir: PathBuf,
    archives_dir: PathBuf,
}

#[derive(Debug)]
struct AlbumInfo {
    name: String,
    encoded_name: String,
    file_count: usize,
    total_size: u64,
    total_size_display: String,
}

#[derive(Debug)]
struct AlbumDetailInfo {
    name: String,
    encoded_name: String,
    files: Vec<FileInfo>,
    total_size: u64,
    total_size_display: String,
}

#[derive(Debug, Clone)]
struct FileInfo {
    name: String,
    size: u64,
    size_display: String,
}

#[derive(Debug)]
struct ArchiveInfo {
    name: String,
    size: u64,
    encoded_name: String,
    size_display: String,
}

#[derive(Debug)]
struct JobEntry {
    id: Uuid,
    playlist_url: String,
    command_line: String,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    status: JobStatus,
    logs: Vec<String>,
    error: Option<String>,
}

impl JobEntry {
    fn new(id: Uuid, playlist_url: String, command_line: String) -> Self {
        Self {
            id,
            playlist_url,
            command_line,
            started_at: Utc::now(),
            finished_at: None,
            status: JobStatus::Pending,
            logs: Vec::new(),
            error: None,
        }
    }

    fn push_line(&mut self, line: impl Into<String>) {
        let mut text = line.into();
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        self.logs.push(text);
        const MAX_LINES: usize = 4000;
        if self.logs.len() > MAX_LINES {
            let extra = self.logs.len() - MAX_LINES;
            self.logs.drain(0..extra);
        }
    }
}

#[derive(Debug, Clone)]
enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobStatus::Pending => write!(f, "pending"),
            JobStatus::Running => write!(f, "running"),
            JobStatus::Completed => write!(f, "completed"),
            JobStatus::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct DownloadForm {
    playlist_url: String,
    format: String,
    extractor_args: String,
    #[serde(default, deserialize_with = "checkbox_bool")]
    force_ipv4: bool,
    #[serde(default, deserialize_with = "checkbox_bool")]
    extract_audio: bool,
    audio_format: String,
    audio_quality: String,
    download_archive: String,
    #[serde(default, deserialize_with = "checkbox_bool")]
    ignore_errors: bool,
    #[serde(default, deserialize_with = "checkbox_bool")]
    embed_metadata: bool,
    #[serde(default, deserialize_with = "checkbox_bool")]
    embed_thumbnail: bool,
    #[serde(default, deserialize_with = "checkbox_bool")]
    add_metadata: bool,
    parse_metadata: String,
    output_template: String,
}

impl Default for DownloadForm {
    fn default() -> Self {
        Self {
            playlist_url:
                "https://www.youtube.com/playlist?list=PLSIh1Vh0_In0Q270LXphs_RxuJBuCT2gy"
                    .to_string(),
            format: r#"bestaudio[ext=m4a]/bestaudio/best"#.to_string(),
            extractor_args: "youtube:player_client=android".to_string(),
            force_ipv4: true,
            extract_audio: true,
            audio_format: "mp3".to_string(),
            audio_quality: "0".to_string(),
            download_archive: "downloaded_%(playlist_title)s.txt".to_string(),
            ignore_errors: true,
            embed_metadata: true,
            embed_thumbnail: true,
            add_metadata: true,
            parse_metadata: "playlist_title:%(album)s".to_string(),
            output_template: "%(playlist_title)s/%(playlist_index)02d - %(title)s.%(ext)s"
                .to_string(),
        }
    }
}

fn checkbox_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value
        .map(|raw| {
            let normalized = raw.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "true" | "1" | "yes" | "on")
        })
        .unwrap_or(false))
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    form_defaults: DownloadForm,
    albums: Vec<AlbumInfo>,
    archives: Vec<ArchiveInfo>,
    jobs: Vec<JobSummary>,
    nav_albums: Vec<AlbumNav>,
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsTemplate {
    jobs: Vec<JobSummary>,
    nav_albums: Vec<AlbumNav>,
}

#[derive(Template)]
#[template(path = "job_detail.html")]
struct JobDetailTemplate {
    job: JobDetail,
    nav_albums: Vec<AlbumNav>,
}

#[derive(Template)]
#[template(path = "album.html")]
struct AlbumTemplate {
    album: AlbumDetailInfo,
    nav_albums: Vec<AlbumNav>,
}

#[derive(Template)]
#[template(path = "archive_edit.html")]
struct ArchiveTemplate {
    archive: ArchiveFile,
    nav_albums: Vec<AlbumNav>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    form: DirectoryPaths,
    nav_albums: Vec<AlbumNav>,
}

#[derive(Debug, Clone)]
struct JobSummary {
    id: Uuid,
    playlist_url: String,
    status: JobStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    started_at_display: String,
    finished_at_display: String,
}

#[derive(Debug)]
struct JobDetail {
    id: Uuid,
    playlist_url: String,
    command_line: String,
    status: JobStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    started_at_display: String,
    finished_at_display: String,
    log_text: String,
    error: Option<String>,
}

#[derive(Debug)]
struct ArchiveFile {
    name: String,
    encoded_name: String,
    content: String,
}

#[derive(Debug, Clone)]
struct AlbumNav {
    name: String,
    encoded_name: String,
}

#[derive(Clone)]
struct DirectoryPaths {
    downloads_dir: String,
    archives_dir: String,
}

impl From<DirectoryConfig> for DirectoryPaths {
    fn from(config: DirectoryConfig) -> Self {
        Self {
            downloads_dir: config.downloads_dir.display().to_string(),
            archives_dir: config.archives_dir.display().to_string(),
        }
    }
}

struct HtmlTemplate<T: Template>(T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => {
                error!("Template error: {}", err);
                StatusCode::INTERNAL_SERVER_ERROR
                    .with_reason("Template rendering failed")
                    .into_response()
            }
        }
    }
}

trait WithReason {
    fn with_reason(self, message: impl Into<String>) -> (StatusCode, String);
}

impl WithReason for StatusCode {
    fn with_reason(self, message: impl Into<String>) -> (StatusCode, String) {
        (self, message.into())
    }
}

async fn home(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let directories = state.config.directories_snapshot().await;
    let (albums_res, archives, jobs) = tokio::join!(
        gather_albums(directories.downloads_dir.clone()),
        gather_archives(directories.archives_dir.clone()),
        gather_job_summaries(state.jobs.clone())
    );
    let albums = albums_res?;
    let nav_albums = albums
        .iter()
        .map(|album| AlbumNav {
            name: album.name.clone(),
            encoded_name: album.encoded_name.clone(),
        })
        .collect();
    let template = HomeTemplate {
        form_defaults: DownloadForm::default(),
        albums,
        archives: archives?,
        jobs: jobs?,
        nav_albums,
    };
    Ok(HtmlTemplate(template))
}

async fn list_jobs(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let jobs = gather_job_summaries(state.jobs.clone()).await?;
    let downloads_dir = state.config.downloads_dir().await;
    let nav_albums = gather_nav_albums(downloads_dir).await?;
    Ok(HtmlTemplate(JobsTemplate { jobs, nav_albums }))
}

async fn job_detail(
    AxumPath(job_id): AxumPath<Uuid>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let job = fetch_job(job_id, state.jobs.clone()).await?;
    let downloads_dir = state.config.downloads_dir().await;
    let nav_albums = gather_nav_albums(downloads_dir).await?;
    Ok(HtmlTemplate(JobDetailTemplate { job, nav_albums }))
}

async fn job_log(
    AxumPath(job_id): AxumPath<Uuid>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let job_arc = {
        let jobs = state.jobs.read().await;
        jobs.get(&job_id).cloned()
    }
    .ok_or(AppError::NotFound)?;
    let logs = job_arc.read().await.logs.join("\n");
    Ok((StatusCode::OK, logs))
}

async fn album_detail(
    AxumPath(album): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let downloads_dir = state.config.downloads_dir().await;
    let album_info = gather_album_detail(downloads_dir.clone(), album.clone()).await?;
    let nav_albums = gather_nav_albums(downloads_dir).await?;
    Ok(HtmlTemplate(AlbumTemplate {
        album: album_info,
        nav_albums,
    }))
}

#[derive(Deserialize)]
struct DeleteFileForm {
    file: String,
}

async fn delete_file(
    AxumPath(album): AxumPath<String>,
    State(state): State<AppState>,
    Form(form): Form<DeleteFileForm>,
) -> Result<impl IntoResponse, AppError> {
    let downloads_dir = state.config.downloads_dir().await;
    let album_path = validate_album(&downloads_dir, &album)?;
    let file_path = sanitize_child_path(&album_path, &form.file)?;
    fs::remove_file(&file_path).await?;
    Ok(Redirect::to(&format!("/albums/{}", encode(&album))))
}

async fn delete_album(
    AxumPath(album): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let downloads_dir = state.config.downloads_dir().await;
    let album_path = validate_album(&downloads_dir, &album)?;
    if album_path.exists() {
        fs::remove_dir_all(&album_path).await?;
    }
    Ok(Redirect::to("/"))
}

async fn edit_archive(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let archives_dir = state.config.archives_dir().await;
    let path = validate_archive(&archives_dir, &name)?;
    let content = if path.exists() {
        fs::read_to_string(&path).await?
    } else {
        String::new()
    };
    let downloads_dir = state.config.downloads_dir().await;
    let nav_albums = gather_nav_albums(downloads_dir).await?;
    Ok(HtmlTemplate(ArchiveTemplate {
        archive: ArchiveFile {
            encoded_name: encode(&name).to_string(),
            name,
            content,
        },
        nav_albums,
    }))
}

#[derive(Deserialize)]
struct ArchiveForm {
    content: String,
}

#[derive(Deserialize)]
struct DirectoryForm {
    downloads_dir: String,
    archives_dir: String,
}

async fn save_archive(
    AxumPath(name): AxumPath<String>,
    State(state): State<AppState>,
    Form(form): Form<ArchiveForm>,
) -> Result<impl IntoResponse, AppError> {
    let archives_dir = state.config.archives_dir().await;
    let path = validate_archive(&archives_dir, &name)?;
    fs::write(&path, form.content).await?;
    let redirect = format!("/archives/{}/edit", encode(&name));
    Ok(Redirect::to(&redirect))
}

async fn show_settings(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let directories = state.config.directories_snapshot().await;
    let nav_albums = gather_nav_albums(directories.downloads_dir.clone()).await?;
    Ok(HtmlTemplate(SettingsTemplate {
        form: DirectoryPaths::from(directories),
        nav_albums,
    }))
}

async fn update_settings(
    State(state): State<AppState>,
    Form(form): Form<DirectoryForm>,
) -> Result<impl IntoResponse, AppError> {
    let downloads_dir = parse_directory_path(&form.downloads_dir, "Downloads directory")?;
    let archives_dir = parse_directory_path(&form.archives_dir, "Archive directory")?;
    state
        .config
        .set_directories(downloads_dir, archives_dir)
        .await?;
    Ok(Redirect::to("/settings"))
}

async fn start_download(
    State(state): State<AppState>,
    Form(form): Form<DownloadForm>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = spawn_download(form.clone(), state.clone()).await?;
    Ok(Redirect::to(&format!("/jobs/{}", job_id)))
}

async fn spawn_download(form: DownloadForm, state: AppState) -> Result<Uuid, AppError> {
    if form.playlist_url.trim().is_empty() {
        return Err(AppError::Invalid("Playlist URL is required".into()));
    }
    let id = Uuid::new_v4();
    let directories = state.config.directories_snapshot().await;
    let command_line = build_command_preview(&form, &state.config, &directories);
    let job = Arc::new(RwLock::new(JobEntry::new(
        id,
        form.playlist_url.clone(),
        command_line,
    )));
    state.jobs.write().await.insert(id, job.clone());
    let config = state.config.clone();
    tokio::spawn(run_download(form, config, directories, job).map(|res| {
        if let Err(err) = res {
            error!("Download task failed: {}", err);
        }
    }));
    Ok(id)
}

async fn run_download(
    form: DownloadForm,
    config: Arc<AppConfig>,
    directories: DirectoryConfig,
    job: Arc<RwLock<JobEntry>>,
) -> Result<(), AppError> {
    let DownloadForm {
        playlist_url,
        format,
        extractor_args,
        force_ipv4,
        extract_audio,
        audio_format,
        audio_quality,
        download_archive,
        ignore_errors,
        embed_metadata,
        embed_thumbnail,
        add_metadata,
        parse_metadata,
        output_template,
    } = form;
    {
        let mut job_mut = job.write().await;
        job_mut.status = JobStatus::Running;
        job_mut.started_at = Utc::now();
    }

    let mut command = Command::new(&config.yt_dlp_path);
    command.current_dir(&config.data_dir);
    command.arg("-f").arg(format).arg("--force-overwrites");
    if !extractor_args.trim().is_empty() {
        command.arg("--extractor-args").arg(extractor_args);
    }
    if force_ipv4 {
        command.arg("--force-ipv4");
    }
    if extract_audio {
        command.arg("-x");
        command.arg("--audio-format").arg(audio_format);
        command.arg("--audio-quality").arg(audio_quality);
    }
    if !download_archive.trim().is_empty() {
        let archive_path = directories
            .archives_dir
            .join(sanitize_file_name(&download_archive));
        command.arg("--download-archive").arg(archive_path);
    }
    if ignore_errors {
        command.arg("--ignore-errors");
    }
    if embed_metadata {
        command.arg("--embed-metadata");
    }
    if embed_thumbnail {
        command.arg("--embed-thumbnail");
    }
    if add_metadata {
        command.arg("--add-metadata");
    }
    if !parse_metadata.trim().is_empty() {
        command.arg("--parse-metadata").arg(parse_metadata);
    }
    let output_path = directories
        .downloads_dir
        .join(output_template)
        .to_string_lossy()
        .to_string();
    command.arg("-o").arg(output_path);
    command.arg(playlist_url);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let mut job_mut = job.write().await;
            job_mut.status = JobStatus::Failed;
            job_mut.error = Some(format!("Failed to start yt-dlp: {}", err));
            return Err(AppError::Io(err));
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_task = stream_output(stdout, job.clone());
    let stderr_task = stream_output(stderr, job.clone());

    let status = child.wait().await?;
    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    let mut job_mut = job.write().await;
    job_mut.finished_at = Some(Utc::now());
    if status.success() {
        job_mut.status = JobStatus::Completed;
    } else {
        job_mut.status = JobStatus::Failed;
        job_mut.error = Some(format!("yt-dlp exited with {}", status));
    }

    Ok(())
}

fn stream_output<R>(
    stream: Option<R>,
    job: Arc<RwLock<JobEntry>>,
) -> Option<tokio::task::JoinHandle<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    stream.map(|stream| {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => job.write().await.push_line(line.clone()),
                    Err(err) => {
                        job.write()
                            .await
                            .push_line(format!("Failed to read process output: {}", err));
                        break;
                    }
                }
            }
        })
    })
}

fn build_command_preview(
    form: &DownloadForm,
    config: &AppConfig,
    directories: &DirectoryConfig,
) -> String {
    let mut parts = vec![config.yt_dlp_path.display().to_string()];
    parts.push("-f".into());
    parts.push(form.format.clone());
    if !form.extractor_args.trim().is_empty() {
        parts.push("--extractor-args".into());
        parts.push(form.extractor_args.clone());
    }
    if form.force_ipv4 {
        parts.push("--force-ipv4".into());
    }
    if form.extract_audio {
        parts.push("-x".into());
        parts.push("--audio-format".into());
        parts.push(form.audio_format.clone());
        parts.push("--audio-quality".into());
        parts.push(form.audio_quality.clone());
    }
    if !form.download_archive.trim().is_empty() {
        let archive_path = directories
            .archives_dir
            .join(sanitize_file_name(&form.download_archive));
        parts.push("--download-archive".into());
        parts.push(archive_path.display().to_string());
    }
    if form.ignore_errors {
        parts.push("--ignore-errors".into());
    }
    if form.embed_metadata {
        parts.push("--embed-metadata".into());
    }
    if form.embed_thumbnail {
        parts.push("--embed-thumbnail".into());
    }
    if form.add_metadata {
        parts.push("--add-metadata".into());
    }
    if !form.parse_metadata.trim().is_empty() {
        parts.push("--parse-metadata".into());
        parts.push(form.parse_metadata.clone());
    }
    let output_path = directories
        .downloads_dir
        .join(form.output_template.clone())
        .display()
        .to_string();
    parts.push("-o".into());
    parts.push(output_path);
    parts.push(form.playlist_url.clone());
    parts.join(" ")
}

async fn gather_albums(dir: PathBuf) -> Result<Vec<AlbumInfo>, AppError> {
    let result = tokio::task::spawn_blocking(move || {
        let mut result = Vec::new();
        if dir.exists() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let (file_count, total_size) = summarize_dir(entry.path())?;
                    result.push(AlbumInfo {
                        encoded_name: encode(&name).to_string(),
                        name,
                        file_count,
                        total_size,
                        total_size_display: format_bytes(total_size),
                    });
                }
            }
        }
        result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok::<_, std::io::Error>(result)
    })
    .await
    .map_err(AppError::from)?;
    result.map_err(AppError::from)
}

async fn gather_nav_albums(dir: PathBuf) -> Result<Vec<AlbumNav>, AppError> {
    let albums = gather_albums(dir).await?;
    Ok(albums
        .into_iter()
        .map(|album| AlbumNav {
            name: album.name,
            encoded_name: album.encoded_name,
        })
        .collect())
}

fn summarize_dir(path: PathBuf) -> Result<(usize, u64), std::io::Error> {
    let mut count = 0;
    let mut size = 0;
    for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            count += 1;
            size += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok((count, size))
}

async fn gather_archives(dir: PathBuf) -> Result<Vec<ArchiveInfo>, AppError> {
    let result = tokio::task::spawn_blocking(move || {
        let mut result = Vec::new();
        if dir.exists() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let size = entry.metadata()?.len();
                    result.push(ArchiveInfo {
                        encoded_name: encode(&name).to_string(),
                        name,
                        size,
                        size_display: format_bytes(size),
                    });
                }
            }
        }
        result.sort_by(|a, b| b.size.cmp(&a.size));
        Ok::<_, std::io::Error>(result)
    })
    .await
    .map_err(AppError::from)?;
    result.map_err(AppError::from)
}

async fn gather_job_summaries(store: JobStore) -> Result<Vec<JobSummary>, AppError> {
    let handles = {
        let jobs = store.read().await;
        jobs.values().cloned().collect::<Vec<_>>()
    };
    let mut result = Vec::with_capacity(handles.len());
    for handle in handles {
        let job = handle.read().await;
        let started_at = job.started_at;
        let finished_at = job.finished_at;
        result.push(JobSummary {
            id: job.id,
            playlist_url: job.playlist_url.clone(),
            status: job.status.clone(),
            started_at,
            finished_at,
            started_at_display: format_timestamp(started_at),
            finished_at_display: format_optional_timestamp(finished_at),
        });
    }
    result.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(result)
}

async fn fetch_job(job_id: Uuid, store: JobStore) -> Result<JobDetail, AppError> {
    let job_arc = {
        let jobs = store.read().await;
        jobs.get(&job_id).cloned()
    }
    .ok_or(AppError::NotFound)?;
    let job = job_arc.read().await;
    let started_at = job.started_at;
    let finished_at = job.finished_at;
    Ok(JobDetail {
        id: job.id,
        playlist_url: job.playlist_url.clone(),
        command_line: job.command_line.clone(),
        status: job.status.clone(),
        started_at,
        finished_at,
        started_at_display: format_timestamp(started_at),
        finished_at_display: format_optional_timestamp(finished_at),
        log_text: job.logs.join("\n"),
        error: job.error.clone(),
    })
}

async fn gather_album_detail(dir: PathBuf, album: String) -> Result<AlbumDetailInfo, AppError> {
    let path = validate_album(&dir, &album)?;
    let result = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut total_size = 0;
        if path.exists() {
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let size = entry.metadata()?.len();
                    files.push(FileInfo {
                        name,
                        size,
                        size_display: format_bytes(size),
                    });
                    total_size += size;
                }
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        let encoded_name = encode(&album).to_string();
        Ok::<_, std::io::Error>(AlbumDetailInfo {
            name: album,
            encoded_name,
            files,
            total_size,
            total_size_display: format_bytes(total_size),
        })
    })
    .await
    .map_err(AppError::from)?;
    result.map_err(AppError::from)
}

fn validate_album(root: &Path, album: &str) -> Result<PathBuf, AppError> {
    let sanitized = sanitize_child_path(root, album)?;
    if !sanitized.starts_with(root) {
        return Err(AppError::Invalid(
            "Album path is outside downloads directory".into(),
        ));
    }
    Ok(sanitized)
}

fn sanitize_child_path(root: &Path, name: &str) -> Result<PathBuf, AppError> {
    if name.contains("..") {
        return Err(AppError::Invalid("Invalid path component".into()));
    }
    if Path::new(name).is_absolute() {
        return Err(AppError::Invalid("Absolute paths are not allowed".into()));
    }
    Ok(root.join(name))
}

fn validate_archive(root: &Path, name: &str) -> Result<PathBuf, AppError> {
    if name.contains('/') || name.contains('\\') {
        return Err(AppError::Invalid(
            "Archive name must not contain path separators".into(),
        ));
    }
    Ok(root.join(name))
}

fn sanitize_file_name(input: &str) -> String {
    input
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect()
}

fn parse_directory_path(input: &str, label: &str) -> Result<PathBuf, AppError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::Invalid(format!("{label} cannot be empty")));
    }
    Ok(PathBuf::from(trimmed))
}

#[derive(Debug, Error)]
enum AppError {
    #[error("Not found")]
    NotFound,
    #[error("Invalid input: {0}")]
    Invalid(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Server error: {0}")]
    Other(String),
}

impl From<tokio::task::JoinError> for AppError {
    fn from(err: tokio::task::JoinError) -> Self {
        AppError::Other(err.to_string())
    }
}

impl From<std::net::AddrParseError> for AppError {
    fn from(err: std::net::AddrParseError) -> Self {
        AppError::Invalid(err.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Invalid(_) => StatusCode::BAD_REQUEST,
            AppError::Io(_) | AppError::Serde(_) | AppError::Other(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, self.to_string()).into_response()
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.2} {}", size, UNITS[unit])
    }
}

fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn format_optional_timestamp(value: Option<DateTime<Utc>>) -> String {
    value.map(format_timestamp).unwrap_or_else(|| "-".into())
}
