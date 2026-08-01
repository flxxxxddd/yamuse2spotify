//! migrate a yandex music library to spotify, downloading what spotify lacks.
//!
//! the shape of a run: pull the library once, match every track against
//! spotify, push what matched, download what did not, then write reports. each
//! step records what it did in a journal, so any of them can be rerun alone and
//! an interrupted run always resumes rather than restarting.

mod auth;
mod cli;
mod config;
mod download;
mod error;
mod matcher;
mod model;
mod pipeline;
mod report;
mod resilience;
mod source;
mod spotify;
mod state;
mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, DownloadArgs, ScopeArgs, ThresholdArgs};
use config::{Paths, Store};
use error::{Error, Result, io};
use model::Library;
use report::{Reports, Summary};
use state::Journal;
use ui::Ui;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Aborted) => {
            eprintln!(
                "\n  прервано. прогресс сохранён — запустите ту же команду, чтобы продолжить."
            );
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("\n  ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

/// everything one invocation needs, assembled once.
struct App {
    paths: Paths,
    store: Store,
    ui: Ui,
    journal: Journal,
    rps: f64,
}

async fn run(cli: Cli) -> Result<()> {
    let paths = Paths::new(&cli.out);
    paths.prepare()?;
    init_logging(&paths.log)?;

    let config_path = match &cli.config {
        Some(path) => path.clone(),
        None => config::default_config_path()?,
    };

    let mut store = Store::load(config_path)?;
    // command-line credentials win over stored ones and are then remembered, so
    // `--spotify-client-id` is needed exactly once.
    if let Some(id) = &cli.spotify_client_id {
        store.config.spotify_client_id = Some(id.clone());
    }
    if let Some(token) = &cli.yandex_token {
        store.config.yandex_token = Some(token.clone());
    }
    if cli.spotify_client_id.is_some() || cli.yandex_token.is_some() {
        store.save()?;
    }

    let mut app = App {
        journal: Journal::load(&paths.state)?,
        ui: Ui::new(!cli.non_interactive, cli.ambiguous.into()),
        paths,
        store,
        rps: cli.rps,
    };

    match cli.command {
        Some(Command::Auth) => cmd_auth(&mut app).await,
        Some(Command::Pull { scope }) => cmd_pull(&mut app, &scope).await,
        Some(Command::Match { thresholds }) => cmd_match(&mut app, &thresholds).await,
        Some(Command::Push) => cmd_push(&mut app).await,
        Some(Command::Download { options }) => cmd_download(&mut app, &options).await,
        Some(Command::Report) => cmd_report(&app),
        Some(Command::Run {
            scope,
            thresholds,
            options,
        }) => cmd_run(&mut app, &scope, &thresholds, &options).await,
        // the bare binary does the whole thing with every default, which is
        // what someone running it for the first time almost always wants.
        None => {
            cmd_run(
                &mut app,
                &ScopeArgs::default(),
                &ThresholdArgs::default(),
                &DownloadArgs::default(),
            )
            .await
        }
    }
}

// ── commands ────────────────────────────────────────────────────────────────

/// authorise both services and report which accounts were reached.
async fn cmd_auth(app: &mut App) -> Result<()> {
    let yandex = auth::yandex::connect(&mut app.store).await?;
    let status = yandex.account_status().await?;
    let login = status
        .account
        .and_then(|a| a.login)
        .unwrap_or_else(|| "?".into());
    app.ui.note(&format!("  яндекс: {login}"));

    let spotify = connect_spotify(app).await?;
    app.ui
        .note(&format!("  spotify: {}", spotify.account_label().await?));

    app.ui.note(&format!(
        "\n  учётные данные сохранены в {}",
        app.store.path().display()
    ));

    Ok(())
}

/// pull the yandex library to disk.
async fn cmd_pull(app: &mut App, scope: &ScopeArgs) -> Result<()> {
    let yandex = auth::yandex::connect(&mut app.store).await?;
    let library = source::pull(&yandex, scope.into(), &app.ui).await?;

    save_library(&app.paths.library, &library)?;
    app.ui.note(&format!(
        "\n  библиотека сохранена: {} треков → {}",
        library.tracks.len(),
        app.paths.library.display()
    ));

    Ok(())
}

/// match the pulled library against spotify.
async fn cmd_match(app: &mut App, thresholds: &ThresholdArgs) -> Result<()> {
    let thresholds = thresholds.resolve()?;
    let library = load_library(&app.paths.library)?;
    let spotify = connect_spotify(app).await?;

    let outcome = pipeline::run_match(
        &library,
        &spotify,
        &mut app.journal,
        &app.ui,
        &app.paths,
        thresholds,
    )
    .await?;

    app.ui.note(&format!(
        "\n  сопоставлено {} · не найдено {}",
        outcome.matched.len(),
        outcome.unmatched.len()
    ));

    write_reports(app, &library, &outcome.errors)?;
    Ok(())
}

/// push what matched into the spotify account.
async fn cmd_push(app: &mut App) -> Result<()> {
    let library = load_library(&app.paths.library)?;
    let spotify = connect_spotify(app).await?;

    let outcome = pipeline::run_push(&library, &spotify, &mut app.journal, &app.ui).await?;

    app.ui.note(&format!(
        "\n  в «любимые»: {} · плейлистов: {} · альбомов: {} · исполнителей: {}",
        outcome.saved, outcome.playlists, outcome.albums, outcome.artists
    ));

    write_reports(app, &library, &outcome.errors)?;
    Ok(())
}

/// download whatever the policy says to.
async fn cmd_download(app: &mut App, options: &DownloadArgs) -> Result<()> {
    let library = load_library(&app.paths.library)?;
    let yandex = auth::yandex::connect(&mut app.store).await?;

    let (rows, errors) = pipeline::run_download(
        &library,
        &yandex,
        &mut app.journal,
        &app.ui,
        &app.paths,
        options.download.into(),
        options.format.into(),
        options.jobs,
    )
    .await?;

    let bytes: u64 = rows.iter().map(|r| r.bytes).sum();
    app.ui.note(&format!(
        "\n  скачано {} файлов ({})",
        rows.len(),
        report::human_bytes(bytes)
    ));

    write_reports(app, &library, &errors)?;
    Ok(())
}

/// regenerate the reports from the journal alone.
fn cmd_report(app: &App) -> Result<()> {
    let library = load_library(&app.paths.library)?;
    let summary = write_reports(app, &library, &[])?;
    print_summary(&app.ui, &summary, &app.paths);
    Ok(())
}

/// the whole migration, in order.
async fn cmd_run(
    app: &mut App,
    scope: &ScopeArgs,
    thresholds: &ThresholdArgs,
    options: &DownloadArgs,
) -> Result<()> {
    // validated before anything touches the network: a typo in a threshold
    // should not be discovered twenty minutes into a pull.
    let thresholds = thresholds.resolve()?;

    app.ui.note("\n▸ авторизация");
    let yandex = auth::yandex::connect(&mut app.store).await?;
    let spotify = connect_spotify(app).await?;

    app.ui.note("\n▸ выгрузка библиотеки из яндекса");
    // an existing dump is reused: a full pull is thousands of requests, and the
    // library is the one thing a rerun should not redo by default.
    let library = if let Ok(existing) = load_library(&app.paths.library) {
        app.ui.note(&format!(
            "  беру уже выгруженную от {} — удалите {}, чтобы обновить",
            existing.pulled_at,
            app.paths.library.display()
        ));
        existing
    } else {
        let pulled = source::pull(&yandex, scope.into(), &app.ui).await?;
        save_library(&app.paths.library, &pulled)?;
        pulled
    };

    app.ui.note("\n▸ поиск соответствий в spotify");
    let matched = pipeline::run_match(
        &library,
        &spotify,
        &mut app.journal,
        &app.ui,
        &app.paths,
        thresholds,
    )
    .await?;

    app.ui.note("\n▸ заливка в spotify");
    let pushed = pipeline::run_push(&library, &spotify, &mut app.journal, &app.ui).await?;

    app.ui.note("\n▸ скачивание того, чего в spotify нет");
    let (_, download_errors) = pipeline::run_download(
        &library,
        &yandex,
        &mut app.journal,
        &app.ui,
        &app.paths,
        options.download.into(),
        options.format.into(),
        options.jobs,
    )
    .await?;

    let mut errors = matched.errors;
    errors.extend(pushed.errors);
    errors.extend(download_errors);

    let summary = write_reports(app, &library, &errors)?;
    print_summary(&app.ui, &summary, &app.paths);

    Ok(())
}

// ── shared plumbing ─────────────────────────────────────────────────────────

/// authorise spotify with whatever credentials are on hand.
async fn connect_spotify(app: &App) -> Result<spotify::Spotify> {
    let client_id = app
        .store
        .config
        .spotify_client_id
        .as_deref()
        .ok_or_else(|| {
            Error::Config(
                "не задан spotify client id — заведите приложение на developer.spotify.com \
                 и передайте --spotify-client-id"
                    .into(),
            )
        })?;

    let client = auth::spotify::connect(
        client_id,
        &app.store.config.redirect_uri,
        &app.paths.spotify_token,
    )
    .await?;

    // a rate-limit wait is announced through the same progress area everything
    // else uses; silence for minutes is indistinguishable from a hang.
    let multi = app.ui.progress_area();
    spotify::Spotify::new(client, app.rps, move |delay| {
        multi.suspend(|| {
            println!(
                "  spotify просит подождать {} с — жду, прогресс сохранён",
                delay.as_secs()
            );
        });
    })
    .await
}

/// read the pulled library.
fn load_library(path: &Path) -> Result<Library> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Config(format!(
                "библиотека не выгружена — сначала `yamuse2spotify pull` (ожидался {})",
                path.display()
            ))
        } else {
            io(path)(e)
        }
    })?;

    Ok(serde_json::from_str(&text)?)
}

/// write the pulled library.
fn save_library(path: &Path, library: &Library) -> Result<()> {
    let text = serde_json::to_string_pretty(library)?;
    std::fs::write(path, text).map_err(io(path))
}

/// rebuild every report from the journal and return the summary.
fn write_reports(app: &App, library: &Library, errors: &[report::ErrorRow]) -> Result<Summary> {
    let reports = Reports::new(&app.paths.reports)?;
    let state = &app.journal.state;

    let (matched, unmatched, downloaded) = report::rebuild(library, state);

    let summary = Summary {
        tracks: library.tracks.len(),
        matched: state.matched_count(),
        reviewed: state.reviewed_count(),
        unmatched: state.unmatched.len(),
        saved: state.saved_tracks.len(),
        playlists: state.playlists.len(),
        albums: state.saved_albums.len(),
        artists: state.followed_artists.len(),
        downloaded: state.downloaded.len(),
        downloaded_bytes: total_downloaded_bytes(app, state),
        errors: errors.len(),
    };

    reports.write_csv("matched.csv", &matched)?;
    reports.write_csv("unmatched.csv", &unmatched)?;
    reports.write_csv("downloaded.csv", &downloaded)?;
    reports.write_csv("errors.csv", errors)?;
    reports.write_summary(&summary, &library.pulled_at)?;

    Ok(summary)
}

/// ask the filesystem how much was downloaded.
///
/// the journal records paths, not sizes — deliberately, so that deleting a file
/// shows up in the next report instead of being contradicted by it.
fn total_downloaded_bytes(app: &App, state: &state::State) -> u64 {
    state
        .downloaded
        .values()
        .filter_map(|relative| std::fs::metadata(app.paths.music.join(relative)).ok())
        .map(|m| m.len())
        .sum()
}

/// the closing block a run prints.
fn print_summary(ui: &Ui, summary: &Summary, paths: &Paths) {
    ui.note(&format!(
        "\n  ─────────────────────────────────────────\n  \
         треков в библиотеке   {}\n  \
         найдено в spotify     {} ({:.1}%)\n  \
         не найдено            {}\n  \
         скачано локально      {} ({})\n  \
         ошибок                {}\n  \
         ─────────────────────────────────────────",
        summary.tracks,
        summary.matched,
        summary.coverage(),
        summary.unmatched,
        summary.downloaded,
        report::human_bytes(summary.downloaded_bytes),
        summary.errors,
    ));

    ui.note(&format!("\n  отчёты: {}", paths.reports.display()));

    if summary.downloaded > 0 {
        ui.note(&format!(
            "  локальные файлы: {}\n  \
             spotify не принимает загрузку аудио через api — добавьте эту папку\n  \
             в настройках десктопного клиента, раздел «локальные файлы».",
            paths.music.display()
        ));
    }
}

/// send tracing to a file rather than the terminal.
///
/// a log line printed mid-run would tear through whatever progress bar is
/// drawing at the time, so nothing goes to stderr; the file is where a failure
/// gets diagnosed after the fact.
fn init_logging(path: &PathBuf) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io(path))?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("yamuse2spotify=info,yamuse=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .init();

    Ok(())
}
