use std::collections::HashMap;
use std::env;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Datelike, Utc};
use clap::Parser;
use color_eyre::{Result, eyre::ErrReport, eyre::eyre};
use crossterm::event::{Event, EventStream, KeyCode};
use futures::future::try_join_all;
use octocrab::{
    Octocrab,
    models::{
        RunId,
        workflows::{Conclusion, Job, Run, Status},
    },
};
use ratatui::DefaultTerminal;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Cell, HighlightSpacing, Paragraph, Row, Table, TableState};
use throbber_widgets_tui::{Throbber, ThrobberState};
use tokio::time::{Instant, interval};
use tokio_stream::StreamExt;
use tracing::{debug, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use unicode_width::UnicodeWidthStr;
use zip::ZipArchive;

#[derive(Parser)]
#[command(version)]
#[command(about = "Monitor GitHub Action workflow runs in the terminal")]
struct Args {
    /// GitHub repository in owner/repo format (defaults to origin of current git repo)
    #[arg(short, long, value_name = "OWNER/REPO")]
    repo: Option<GitHubRepo>,

    /// Filter to specific branches (defaults to all branches)
    #[arg(short, long, value_delimiter = ',')]
    branch: Option<Vec<String>>,

    /// Output logs to a file
    #[arg(short, long, value_name = "FILE")]
    log: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let _guard = match &args.log {
        Some(log_path) => {
            let log_dir = log_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let log_file = log_path.file_name().unwrap().to_str().unwrap();
            let (file_appender, guard) =
                tracing_appender::non_blocking(tracing_appender::rolling::never(log_dir, log_file));
            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new("gal=debug,warn"))
                .with(tracing_subscriber::fmt::layer().with_writer(file_appender))
                .init();
            Some(guard)
        }
        _ => None,
    };

    debug!("initialising app");
    color_eyre::install()?;
    let terminal = ratatui::init();
    let app_result = App::new(args).run(terminal).await;
    ratatui::restore();
    app_result
}

#[derive(Debug)]
struct App {
    workflow_runs: WorkflowRunsListWidget,
    should_quit: bool,
    last_interaction: Instant,
}

impl App {
    const FRAMES_PER_SECOND: f32 = 60.0;
    const IDLE_FRAMES_PER_SECOND: f32 = 1.0;
    const THROBBER_FRAMES_PER_SECOND: f32 = 20.0;
    const INTERACTION_TIMEOUT: Duration = Duration::from_millis(200);

    pub fn new(args: Args) -> Self {
        let repo = match args.repo {
            Some(repo) => repo,
            None => parse_github_repo(&get_git_origin_url().unwrap()).unwrap(),
        };

        let octocrab = match env::var("GITHUB_TOKEN") {
            Ok(token) => octocrab::Octocrab::builder()
                .personal_token(token.to_owned())
                .build()
                .unwrap(),
            Err(_) => octocrab::Octocrab::default(),
        };

        let workflow_runs = WorkflowRunsListWidget::new(octocrab, repo, args.branch);

        Self {
            workflow_runs,
            should_quit: false,
            last_interaction: Instant::now(),
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        self.workflow_runs.run();

        let period = Duration::from_secs_f32(1.0 / Self::FRAMES_PER_SECOND);
        let idle_period = Duration::from_secs_f32(1.0 / Self::IDLE_FRAMES_PER_SECOND);
        let throbber_period = Duration::from_secs_f32(1.0 / Self::THROBBER_FRAMES_PER_SECOND);
        let mut normal_interval = tokio::time::interval(period);
        let mut idle_interval = tokio::time::interval(idle_period);
        let mut throbber_interval = tokio::time::interval(throbber_period);
        let mut events = EventStream::new();

        while !self.should_quit {
            let is_idle = !self.workflow_runs.has_data_updates()
                && self.last_interaction.elapsed() > Self::INTERACTION_TIMEOUT;
            tokio::select! {
                _ = normal_interval.tick(), if !is_idle => {
                    terminal.draw(|frame| self.render(frame))?;
                    self.workflow_runs.set_data_updated(false);
                },
                _ = idle_interval.tick(), if is_idle => {
                    terminal.draw(|frame| self.render(frame))?;
                    self.workflow_runs.set_data_updated(false);
                },
                _ = throbber_interval.tick()=> self.workflow_runs.update_throbber(),
                Some(Ok(event)) = events.next() => {
                    self.handle_event(&event);
                    self.last_interaction = Instant::now();
                }
            }
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        frame.render_widget(&self.workflow_runs, frame.area());
    }

    fn handle_event(&mut self, event: &Event) {
        if let Some(key) = event.as_key_press_event() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                KeyCode::Char('j') | KeyCode::Down => self.workflow_runs.scroll_down(),
                KeyCode::Char('k') | KeyCode::Up => self.workflow_runs.scroll_up(),
                KeyCode::Enter => self.workflow_runs.open_selected_workflow(),
                KeyCode::Char(' ') => {
                    let runs = Arc::new(self.workflow_runs.clone());
                    runs.toggle_job_breakdown_pane()
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
struct WorkflowRunsListWidget {
    state: Arc<RwLock<WorkflowRunsListState>>,
    client: Octocrab,
    repo: GitHubRepo,
    branches: Option<Vec<String>>,
}

#[derive(Debug, Default)]
struct WorkflowRunsListState {
    workflow_runs: Vec<WorkflowRun>,
    loading_state: LoadingState,
    table_state: TableState,
    throbber_state: ThrobberState,
    completed_workflow_jobs: HashMap<u64, Vec<Job>>,
    data_updated: bool,
    show_job_breakdown_pane: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum LoadingState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FailedStepRef {
    job_id: u64,
    job_name: String,
    step_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FailedStepLogTail {
    failed_step: FailedStepRef,
    lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FailedStepLogState {
    NotAvailable,
    NotLoaded,
    Loading(FailedStepRef),
    Loaded(FailedStepLogTail),
    LoadingError(FailedStepRef, String),
}

impl WorkflowRunsListWidget {
    const FAILED_LOG_TAIL_LINES: usize = 100;

    fn has_data_updates(&self) -> bool {
        self.state.read().unwrap().data_updated
    }

    fn set_data_updated(&self, data_updated: bool) {
        self.state.write().unwrap().data_updated = data_updated;
    }

    fn new(client: Octocrab, repo: GitHubRepo, branches: Option<Vec<String>>) -> Self {
        let mut state = WorkflowRunsListState::default();
        state.show_job_breakdown_pane = true;
        Self {
            state: Arc::new(RwLock::new(state)),
            client,
            repo,
            branches,
        }
    }

    fn run(&self) {
        let this = Arc::new(self.clone());
        tokio::spawn(this.fetch_runs());
    }

    async fn fetch_runs(self: Arc<Self>) {
        let mut interval = interval(Duration::from_secs(10));
        loop {
            self.set_loading_state(LoadingState::Loading);
            let this = self.clone();
            match self.get_detailed_workflow_runs().await {
                Ok(runs) => this.on_load(runs),
                Err(err) => self.on_err(&err),
            }
            interval.tick().await;
        }
    }

    fn on_load(self: Arc<Self>, runs: Vec<WorkflowRun>) {
        let mut state = self.state.write().unwrap();
        state.loading_state = LoadingState::Loaded;
        let was_empty = state.workflow_runs.is_empty();
        state.workflow_runs = runs;
        state.data_updated = true;
        if !state.workflow_runs.is_empty() && was_empty {
            state.table_state.select(Some(0));
        }
        drop(state);
        self.clone().ensure_selected_workflow_jobs();
        self.ensure_selected_failed_step_log();
    }

    fn on_err(&self, err: &ErrReport) {
        self.set_loading_state(LoadingState::Error(format!("{err:?}")));
    }

    fn set_loading_state(&self, state: LoadingState) {
        self.state.write().unwrap().loading_state = state;
    }

    fn update_throbber(&self) {
        let mut state = self.state.write().unwrap();
        if state.loading_state == LoadingState::Loading {
            state.throbber_state.calc_next();
            state.data_updated = true;
        }
    }

    fn scroll_down(&self) {
        let should_load = {
            let mut state = self.state.write().unwrap();
            state.table_state.scroll_down_by(1);
            state.show_job_breakdown_pane
        };
        if should_load {
            let this = Arc::new(self.clone());
            this.clone().ensure_selected_workflow_jobs();
            this.ensure_selected_failed_step_log();
        }
    }

    fn scroll_up(&self) {
        let should_load = {
            let mut state = self.state.write().unwrap();
            state.table_state.scroll_up_by(1);
            state.show_job_breakdown_pane
        };
        if should_load {
            let this = Arc::new(self.clone());
            this.clone().ensure_selected_workflow_jobs();
            this.ensure_selected_failed_step_log();
        }
    }

    async fn get_detailed_workflow_runs(&self) -> Result<Vec<WorkflowRun>> {
        let workflows = match self.get_workflow_runs().await {
            Ok(runs) => runs,
            Err(err) => {
                error!("{:?}", err);
                return Err(err);
            }
        };
        let existing_workflow_runs = Arc::new({
            let state = self.state.read().unwrap();
            state
                .workflow_runs
                .iter()
                .map(|run| (run.id, (run.jobs.clone(), run.failed_step_log.clone())))
                .collect::<HashMap<RunId, (JobsState, FailedStepLogState)>>()
        });
        let details_futures = workflows.into_iter().map(|run| {
            let existing_workflow_runs = existing_workflow_runs.clone();
            async move {
                WorkflowRun {
                    id: run.id,
                    name: run.name,
                    commit: run.head_commit.message,
                    branch: run.head_branch.to_string(),
                    status: run.status,
                    conclusion: run.conclusion,
                    jobs: match existing_workflow_runs.get(&run.id) {
                        Some((jobs, _)) => jobs.clone(),
                        None => JobsState::NotLoaded,
                    },
                    failed_step_log: match existing_workflow_runs.get(&run.id) {
                        Some((_, failed_step_log)) => failed_step_log.clone(),
                        None => FailedStepLogState::NotLoaded,
                    },
                    created_at: run.created_at,
                    html_url: run.html_url.clone().into(),
                }
            }
        });
        let details = futures::future::join_all(details_futures).await;
        Ok(details)
    }

    async fn get_workflow_jobs(&self, run_id: RunId, is_completed: bool) -> Result<Vec<Job>> {
        let existing_jobs = {
            let state = self.state.read().unwrap();
            state.completed_workflow_jobs.get(&run_id.0).cloned()
        };
        let jobs = match existing_jobs {
            Some(jobs) => jobs,
            None => {
                debug!("fetching jobs for workflow run {}", run_id.0);
                let jobs = self
                    .client
                    .workflows(&self.repo.owner, &self.repo.name)
                    .list_jobs(run_id)
                    .send()
                    .await?
                    .items;
                if is_completed {
                    self.state
                        .write()
                        .unwrap()
                        .completed_workflow_jobs
                        .insert(run_id.0, jobs.clone());
                }
                jobs
            }
        };
        Ok(jobs)
    }

    async fn get_workflow_runs(&self) -> Result<Vec<Run>> {
        debug!("fetching workflow runs");
        let runs = match &self.branches {
            Some(branches) => {
                let futures = branches.iter().map(|branch| async move {
                    self.client
                        .workflows(&self.repo.owner, &self.repo.name)
                        .list_all_runs()
                        .branch(branch)
                        .send()
                        .await
                        .map(|resp| resp.items)
                });
                let results = try_join_all(futures).await?;
                let mut runs = results.into_iter().flatten().collect::<Vec<_>>();
                runs.sort_unstable_by(|a, b| b.id.0.cmp(&a.id.0));
                runs
            }
            None => {
                self.client
                    .workflows(&self.repo.owner, &self.repo.name)
                    .list_all_runs()
                    .send()
                    .await?
                    .items
            }
        };
        Ok(runs)
    }

    fn toggle_job_breakdown_pane(self: Arc<Self>) {
        let mut state = self.state.write().unwrap();
        state.show_job_breakdown_pane = !state.show_job_breakdown_pane;
        state.data_updated = true;
        let should_load = state.show_job_breakdown_pane;
        drop(state);
        if should_load {
            self.clone().ensure_selected_workflow_jobs();
            self.ensure_selected_failed_step_log();
        }
    }

    fn ensure_selected_workflow_jobs(self: Arc<Self>) {
        let (run_id, is_completed) = {
            let mut state = self.state.write().unwrap();
            let Some(selected_index) = state.table_state.selected() else {
                return;
            };
            let Some(run) = state.workflow_runs.get_mut(selected_index) else {
                return;
            };
            let should_fetch = match &run.jobs {
                JobsState::Loading | JobsState::Reloading(_) => false,
                JobsState::NotLoaded => {
                    run.jobs = JobsState::Loading;
                    true
                }
                JobsState::LoadingError(_) => {
                    run.jobs = JobsState::Loading;
                    true
                }
                JobsState::Loaded(jobs) if run.status.as_str() == "in_progress" => {
                    run.jobs = JobsState::Reloading(jobs.clone());
                    true
                }
                JobsState::Loaded(_) => false,
            };
            if !should_fetch {
                return;
            }
            (run.id, run.status.as_str() == "completed")
        };
        let this = self.clone();
        let state_arc = self.state.clone();
        tokio::spawn(async move {
            let jobs_state = match this.get_workflow_jobs(run_id, is_completed).await {
                Ok(jobs) => JobsState::Loaded(jobs),
                Err(err) => JobsState::LoadingError(err.to_string()),
            };
            let mut refresh_failed_step_logs = false;
            let mut state = state_arc.write().unwrap();
            if let Some(run) = state.workflow_runs.iter_mut().find(|run| run.id == run_id) {
                run.jobs = jobs_state;
                state.data_updated = true;
                refresh_failed_step_logs = true;
            }
            drop(state);
            if refresh_failed_step_logs {
                this.ensure_selected_failed_step_log();
            }
        });
    }

    fn ensure_selected_failed_step_log(self: Arc<Self>) {
        let selected = {
            let mut state = self.state.write().unwrap();
            let Some(selected_index) = state.table_state.selected() else {
                return;
            };
            let Some(run) = state.workflow_runs.get(selected_index) else {
                return;
            };
            let run_id = run.id;
            let failed_step = run.first_failed_step();
            let current_failed_step_log = run.failed_step_log.clone();
            let Some(failed_step) = failed_step else {
                if current_failed_step_log != FailedStepLogState::NotAvailable {
                    if let Some(run) = state.workflow_runs.get_mut(selected_index) {
                        run.failed_step_log = FailedStepLogState::NotAvailable;
                    }
                    state.data_updated = true;
                }
                return;
            };
            let should_fetch = match &current_failed_step_log {
                FailedStepLogState::Loading(current) => current != &failed_step,
                FailedStepLogState::Loaded(current) => current.failed_step != failed_step,
                FailedStepLogState::LoadingError(current, _) => current != &failed_step,
                FailedStepLogState::NotLoaded | FailedStepLogState::NotAvailable => true,
            };
            if !should_fetch {
                return;
            }
            if let Some(run) = state.workflow_runs.get_mut(selected_index) {
                run.failed_step_log = FailedStepLogState::Loading(failed_step.clone());
            }
            state.data_updated = true;
            (run_id, failed_step)
        };

        let this = self.clone();
        let state_arc = self.state.clone();
        tokio::spawn(async move {
            let (run_id, failed_step) = selected;
            let log_state = match this.get_failed_step_log_tail(run_id, &failed_step).await {
                Ok(lines) => FailedStepLogState::Loaded(FailedStepLogTail { failed_step, lines }),
                Err(err) => FailedStepLogState::LoadingError(failed_step, err.to_string()),
            };
            let mut state = state_arc.write().unwrap();
            if let Some(run) = state.workflow_runs.iter_mut().find(|run| run.id == run_id) {
                run.failed_step_log = log_state;
                state.data_updated = true;
            }
        });
    }

    async fn get_failed_step_log_tail(
        &self,
        run_id: RunId,
        failed_step: &FailedStepRef,
    ) -> Result<Vec<String>> {
        let logs_zip = self
            .client
            .actions()
            .download_workflow_run_logs(&self.repo.owner, &self.repo.name, run_id)
            .await?;
        let job_log = extract_job_log_text_from_archive(logs_zip.as_ref(), failed_step)?;
        Ok(extract_step_log_tail(
            &job_log,
            &failed_step.step_name,
            Self::FAILED_LOG_TAIL_LINES,
        ))
    }

    fn open_selected_workflow(&self) {
        let state = self.state.read().unwrap();
        if let Some(selected_index) = state.table_state.selected()
            && let Some(run) = state.workflow_runs.get(selected_index)
        {
            let url = &run.html_url;
            debug!("opening URL: {}", url);

            if let Err(e) = webbrowser::open(url) {
                error!("Failed to open browser: {}", e);
            }
        }
    }

    fn calculate_column_widths(&self, headers: &[&str], rows: &[RunRow]) -> Vec<Constraint> {
        let mut max_widths = headers
            .iter()
            .take(2)
            .map(|h| UnicodeWidthStr::width(*h))
            .collect::<Vec<_>>();

        for row in rows {
            max_widths[0] = max_widths[0].max(UnicodeWidthStr::width(row.time.as_str()));
            max_widths[1] = max_widths[1]
                .max(UnicodeWidthStr::width(row.branch.as_str()))
                .min(25);
        }

        max_widths
            .into_iter()
            .map(|mw| Constraint::Length(mw as u16 + 1))
            .chain(std::iter::once(Constraint::Min(20)))
            .collect()
    }
}

impl Widget for &WorkflowRunsListWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut state = self.state.write().unwrap();

        let white = Style::default().fg(Color::White);
        let mut table_block =
            Block::bordered()
                .title(self.repo.full_name())
                .title_bottom(Line::from(vec![
                    Span::styled("j/k", white),
                    Span::from(" up/down  "),
                    Span::styled("space", white),
                    Span::from(" toggle job pane  "),
                    Span::styled("enter", white),
                    Span::from(" open browser  "),
                    Span::styled("q", white),
                    Span::from(" quit"),
                ]));

        table_block = match &state.loading_state {
            LoadingState::Loading => {
                let throbber = Throbber::default().throbber_set(throbber_widgets_tui::BRAILLE_ONE);
                let throbber_span = throbber.to_symbol_span(&state.throbber_state);
                let throbber_text = throbber_span.content.as_ref().trim_end().to_string();
                table_block.title(Line::from(throbber_text).right_aligned())
            }
            LoadingState::Error(_) => table_block.title(Line::from("Error").right_aligned()),
            _ => table_block,
        };

        let (table_area, jobs_area) = if state.show_job_breakdown_pane {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        if state.workflow_runs.is_empty() {
            let loading_message = match &state.loading_state {
                LoadingState::Loading => "Loading workflow runs from GitHub...",
                LoadingState::Error(err) => err.as_str(),
                LoadingState::Idle => "No workflow runs found.",
                LoadingState::Loaded => "No workflow runs available.",
            };

            let paragraph = Paragraph::new(Text::from(loading_message))
                .block(table_block)
                .wrap(ratatui::widgets::Wrap { trim: true });

            paragraph.render(table_area, buf);
        } else {
            let headers = ["Time", "Branch", "Run"];
            let header = headers
                .into_iter()
                .map(Cell::from)
                .collect::<Row>()
                .style(Style::new().bold())
                .height(1);
            let max_height = table_area.height as i32 - 3; // 1 header and 2 border
            let offset = state.table_state.offset();
            let mut consumed_height = 0;

            let run_rows = state
                .workflow_runs
                .iter()
                .map(|run| run.to_row())
                .collect::<Vec<RunRow>>();
            let headers = ["Time", "Branch", "Run"];
            let widths = self.calculate_column_widths(&headers, &run_rows);
            let rows = run_rows
                .into_iter()
                .enumerate()
                .map(|(i, run)| {
                    let row_height = run.details_lines.len() as i32;
                    let visible_height = if i < offset {
                        row_height
                    } else if consumed_height < max_height
                        && consumed_height + row_height > max_height
                    {
                        let partial_height = max_height - consumed_height;
                        consumed_height = max_height;
                        partial_height
                    } else {
                        consumed_height += row_height;
                        row_height
                    };
                    let row: Row = Row::new(vec![
                        Cell::from(run.time.clone()),
                        Cell::from(run.branch.clone()),
                        Cell::from(run.details_lines.clone()),
                    ]);
                    row.height(visible_height as u16)
                })
                .collect::<Vec<_>>();
            let table = Table::new(rows, widths)
                .block(table_block)
                .header(header)
                .highlight_spacing(HighlightSpacing::Always)
                .highlight_symbol("> ")
                .row_highlight_style(Style::new().on_black().bold());

            StatefulWidget::render(table, table_area, buf, &mut state.table_state);
        }

        if let Some(jobs_area) = jobs_area {
            let selected_run = state
                .table_state
                .selected()
                .and_then(|selected_index| state.workflow_runs.get(selected_index));
            let jobs_lines = selected_run.map_or_else(
                || vec![Line::from("No workflow selected")],
                WorkflowRun::job_breakdown_lines,
            );
            let show_failed_logs = selected_run
                .map(WorkflowRun::has_failed_step)
                .unwrap_or(false);
            let (job_breakdown_area, failed_log_area) = if show_failed_logs {
                let needed_job_height = (jobs_lines.len() as u16).saturating_add(2);
                if needed_job_height >= jobs_area.height {
                    (jobs_area, None)
                } else {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(needed_job_height), Constraint::Min(0)])
                        .split(jobs_area);
                    (chunks[0], Some(chunks[1]))
                }
            } else {
                (jobs_area, None)
            };

            let jobs_paragraph = Paragraph::new(Text::from(jobs_lines))
                .block(Block::bordered().title("Job Breakdown"));
            jobs_paragraph.render(job_breakdown_area, buf);

            if let Some(failed_log_area) = failed_log_area {
                let failed_log_lines = selected_run.map_or_else(
                    || vec![Line::from("No failed step selected")],
                    WorkflowRun::failed_step_log_lines,
                );
                let visible_lines = failed_log_area.height.saturating_sub(2) as usize;
                let failed_log_lines =
                    if visible_lines == 0 || failed_log_lines.len() <= visible_lines {
                        failed_log_lines
                    } else {
                        failed_log_lines[failed_log_lines.len() - visible_lines..].to_vec()
                    };
                let failed_log_paragraph = Paragraph::new(Text::from(failed_log_lines))
                    .block(Block::bordered().title("Failed Step Log Tail"))
                    .wrap(ratatui::widgets::Wrap { trim: false });
                failed_log_paragraph.render(failed_log_area, buf);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct WorkflowRun {
    id: RunId,
    name: String,
    commit: String,
    branch: String,
    status: String,
    conclusion: Option<String>,
    jobs: JobsState,
    failed_step_log: FailedStepLogState,
    #[allow(dead_code)]
    created_at: DateTime<Utc>,
    html_url: String,
}

#[derive(Debug, Clone, PartialEq)]
enum JobsState {
    NotLoaded,
    Loading,
    Loaded(Vec<Job>),
    Reloading(Vec<Job>),
    LoadingError(String),
}

impl WorkflowRun {
    fn to_row(&self) -> RunRow {
        let run = self;

        let details_lines = vec![{
            let status_symbol = get_run_status_symbol(&run.status, &run.conclusion);
            Line::styled(
                format!("{} {} - {}", status_symbol.symbol, run.name, run.commit),
                Style::default().fg(status_symbol.color),
            )
        }];

        RunRow {
            time: format_relative_time(run.created_at),
            branch: run.branch.clone(),
            details_lines,
        }
    }

    fn job_breakdown_lines(&self) -> Vec<Line<'static>> {
        match &self.jobs {
            JobsState::NotLoaded => vec![Line::from("Loading not started")],
            JobsState::Loading => vec![Line::from("Loading jobs...")],
            JobsState::LoadingError(err) => vec![Line::from(format!("Error loading jobs: {err}"))],
            JobsState::Loaded(jobs) | JobsState::Reloading(jobs) => {
                if jobs.is_empty() {
                    return vec![Line::from("No jobs in this run")];
                }
                let mut all_items = vec![Line::styled(
                    format!("{} ({})", self.name, self.id.0),
                    Style::default().fg(Color::White).bold(),
                )];

                for (job_index, job) in jobs.iter().enumerate() {
                    let is_last_job = job_index == jobs.len() - 1;
                    let job_prefix = if is_last_job { "└─ " } else { "├─ " };
                    let job_status = get_job_status_symbol(&job.status, &job.conclusion);
                    let duration = match (&job.status, job.completed_at) {
                        (Status::InProgress, _) => format_duration(job.started_at, Utc::now()),
                        (_, Some(completed)) => format_duration(job.started_at, completed),
                        _ => String::new(),
                    };
                    all_items.push(Self::job_line(
                        job_prefix.into(),
                        format!("{} {}", job_status.symbol, job.name),
                        duration,
                        job_status.color,
                    ));

                    for (step_index, step) in job.steps.iter().enumerate() {
                        let is_last_step = step_index == job.steps.len() - 1;
                        let step_prefix = if is_last_job {
                            if is_last_step {
                                "   └─ "
                            } else {
                                "   ├─ "
                            }
                        } else if is_last_step {
                            "│  └─ "
                        } else {
                            "│  ├─ "
                        };

                        let status = get_job_status_symbol(&step.status, &step.conclusion);
                        let duration = match (&step.status, step.started_at, step.completed_at) {
                            (Status::InProgress, Some(started), _) => {
                                format_duration(started, Utc::now())
                            }
                            (_, Some(started), Some(completed)) => {
                                format_duration(started, completed)
                            }
                            _ => String::new(),
                        };
                        all_items.push(Self::job_line(
                            step_prefix.into(),
                            format!("{} {}", status.symbol, step.name),
                            duration,
                            status.color,
                        ));
                    }
                }
                all_items
            }
        }
    }

    fn has_failed_step(&self) -> bool {
        self.first_failed_step().is_some()
    }

    fn first_failed_step(&self) -> Option<FailedStepRef> {
        match &self.jobs {
            JobsState::Loaded(jobs) | JobsState::Reloading(jobs) => jobs.iter().find_map(|job| {
                job.steps.iter().find_map(|step| {
                    let failed = matches!(
                        step.conclusion.as_ref(),
                        Some(
                            Conclusion::Failure | Conclusion::TimedOut | Conclusion::ActionRequired
                        )
                    );
                    if failed {
                        Some(FailedStepRef {
                            job_id: job.id.0,
                            job_name: job.name.clone(),
                            step_name: step.name.clone(),
                        })
                    } else {
                        None
                    }
                })
            }),
            _ => None,
        }
    }

    fn failed_step_log_lines(&self) -> Vec<Line<'static>> {
        match &self.failed_step_log {
            FailedStepLogState::NotAvailable => vec![Line::from("No failed step in selected run")],
            FailedStepLogState::NotLoaded => vec![Line::from("Log tail not loaded")],
            FailedStepLogState::Loading(failed_step) => vec![
                Line::styled(
                    format!("{} / {}", failed_step.job_name, failed_step.step_name),
                    Style::default().fg(Color::White).bold(),
                ),
                Line::from("Loading failed-step logs..."),
            ],
            FailedStepLogState::LoadingError(failed_step, err) => vec![
                Line::styled(
                    format!("{} / {}", failed_step.job_name, failed_step.step_name),
                    Style::default().fg(Color::White).bold(),
                ),
                Line::from(format!("Error loading logs: {err}")),
            ],
            FailedStepLogState::Loaded(tail) => {
                let mut lines = vec![Line::styled(
                    format!(
                        "{} / {}",
                        tail.failed_step.job_name, tail.failed_step.step_name
                    ),
                    Style::default().fg(Color::White).bold(),
                )];
                if tail.lines.is_empty() {
                    lines.push(Line::from("No log lines found for failed step"));
                    return lines;
                }
                lines.extend(tail.lines.iter().cloned().map(Line::from));
                lines
            }
        }
    }

    fn job_line(prefix: String, text: String, suffix: String, color: Color) -> Line<'static> {
        Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::DarkGray)),
            Span::styled(text, Style::default().fg(color)),
            Span::styled(format!(" {}", suffix), Style::default().fg(Color::DarkGray)),
        ])
    }
}

#[derive(Debug)]
struct RunRow {
    time: String,
    branch: String,
    details_lines: Vec<Line<'static>>,
}

struct StatusDisplay {
    symbol: &'static str,
    color: Color,
}

impl From<(&'static str, Color)> for StatusDisplay {
    fn from(tuple: (&'static str, Color)) -> Self {
        StatusDisplay {
            symbol: tuple.0,
            color: tuple.1,
        }
    }
}

fn format_duration(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let total_seconds = end.signed_duration_since(start).num_seconds().abs();

    if total_seconds < 60 {
        format!("{total_seconds}s")
    } else {
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{minutes}m {seconds}s")
    }
}

fn get_run_status_symbol(status: &str, conclusion: &Option<String>) -> StatusDisplay {
    match status {
        "in_progress" => ("⏵", Color::Yellow),
        "queued" => ("⏸", Color::Blue),
        "pending" => ("⏸", Color::Blue),
        "completed" => match conclusion.as_ref().map(|s| s.as_str()) {
            Some("success") => ("✔", Color::Green),
            Some("failure") => ("⚠", Color::Red),
            Some("timed_out") => ("⚠", Color::Red),
            Some("cancelled") => ("∅", Color::Gray),
            Some("skipped") => ("⏭", Color::Magenta),
            _ => ("⏺", Color::Green),
        },
        "failed" => ("⚠", Color::Red),
        _ => ("?", Color::Magenta),
    }
    .into()
}

fn get_job_status_symbol(status: &Status, conclusion: &Option<Conclusion>) -> StatusDisplay {
    match status {
        Status::InProgress => ("⏵", Color::Yellow),
        Status::Queued => ("⏸", Color::Blue),
        Status::Pending => ("⏸", Color::Blue),
        Status::Completed => match conclusion.as_ref() {
            Some(Conclusion::Success) => ("✔", Color::Green),
            Some(Conclusion::Failure) => ("⚠", Color::Red),
            Some(Conclusion::TimedOut) => ("⚠", Color::Red),
            Some(Conclusion::Cancelled) => ("∅", Color::Gray),
            Some(Conclusion::Skipped) => ("⏭", Color::Magenta),
            _ => ("⏺", Color::Green),
        },
        Status::Failed => ("⚠", Color::Red),
        _ => ("?", Color::Magenta),
    }
    .into()
}

fn extract_job_log_text_from_archive(
    archive_bytes: &[u8],
    failed_step: &FailedStepRef,
) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(archive_bytes))?;
    let mut best_match: Option<(i32, String)> = None;
    let job_name = normalize_log_match_key(&failed_step.job_name);
    let step_name = normalize_log_match_key(&failed_step.step_name);
    let job_id = failed_step.job_id.to_string();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();
        if !name.ends_with(".txt") {
            continue;
        }
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)?;
        let content = String::from_utf8_lossy(&raw).into_owned();
        let mut score = 0;
        let normalized_name = normalize_log_match_key(&name);
        let normalized_content = normalize_log_match_key(&content);
        if normalized_name.contains(&job_name) {
            score += 8;
        }
        if normalized_name.contains(&job_id) {
            score += 4;
        }
        if normalized_content.contains(&step_name) {
            score += 4;
        }
        if normalized_content.contains(&job_name) {
            score += 2;
        }
        let score = score + (content.len() as i32 / 20000);
        match &best_match {
            Some((best_score, _)) if *best_score >= score => {}
            _ => best_match = Some((score, content)),
        }
    }

    best_match
        .map(|(_, content)| content)
        .ok_or_else(|| eyre!("No matching job log found in workflow log archive"))
}

fn extract_step_log_tail(job_log: &str, step_name: &str, max_lines: usize) -> Vec<String> {
    let lines: Vec<&str> = job_log.lines().collect();
    if lines.is_empty() {
        return vec![];
    }
    let step_marker = format!("run {step_name}");
    let marker_key = normalize_log_match_key(&step_marker);
    let start_idx = lines
        .iter()
        .position(|line| normalize_log_match_key(line).contains(&marker_key))
        .or_else(|| {
            let step_key = normalize_log_match_key(step_name);
            lines
                .iter()
                .position(|line| normalize_log_match_key(line).contains(&step_key))
        })
        .unwrap_or(0);
    let end_idx = lines
        .iter()
        .enumerate()
        .skip(start_idx + 1)
        .find(|(_, line)| line.starts_with("##[group]Run "))
        .map(|(idx, _)| idx)
        .unwrap_or(lines.len());
    let mut selected = lines[start_idx..end_idx]
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    while selected.last().is_some_and(|line| line.trim().is_empty()) {
        selected.pop();
    }
    if selected.len() > max_lines {
        selected = selected[selected.len() - max_lines..].to_vec();
    }
    selected
}

fn normalize_log_match_key(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// fn format_duration(started_at: &DateTime<Utc>, completed_at: &Option<DateTime<Utc>>) -> String {
//     match completed_at {
//         Some(end_time) => {
//             let duration = end_time.signed_duration_since(started_at);
//             let total_seconds = duration.num_seconds();
//             let minutes = total_seconds / 60;
//             let seconds = total_seconds % 60;
//             if minutes > 0 {
//                 format!("{minutes}m {seconds}s")
//             } else {
//                 format!("{seconds}s")
//             }
//         }
//         None => "Running...".to_string(),
//     }
// }

fn get_git_origin_url() -> Result<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()?;

    if !output.status.success() {
        return Err(eyre!(
            "Failed to get git origin URL. Make sure you're in a git repository."
        ));
    }

    let url = String::from_utf8(output.stdout)?.trim().to_string();

    if url.is_empty() {
        return Err(eyre!(
            "No origin URL found. Make sure the repository has a remote origin."
        ));
    }

    Ok(url)
}

fn parse_github_repo(url: &str) -> Result<GitHubRepo> {
    let cleaned_url = if url.starts_with("git@github.com:") {
        url.strip_prefix("git@github.com:")
            .ok_or(eyre!("Invalid SSH URL format"))?
    } else if url.starts_with("https://github.com/") {
        url.strip_prefix("https://github.com/")
            .ok_or(eyre!("Invalid HTTPS URL format"))?
    } else {
        return Err(eyre!("URL must be a GitHub repository (SSH or HTTPS)"));
    };

    let repo_path = cleaned_url.strip_suffix(".git").unwrap_or(cleaned_url);

    let parts: Vec<&str> = repo_path.split('/').collect();
    if parts.len() != 2 {
        return Err(eyre!("Invalid repository path format. Expected owner/repo"));
    }

    Ok(GitHubRepo::new(parts[0].to_string(), parts[1].to_string()))
}

fn format_relative_time(dt: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(dt);

    let seconds = diff.num_seconds();
    let days = diff.num_days();

    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", diff.num_minutes()),
        3600..=86399 => format!("{}h ago", diff.num_hours()),
        86400..=604799 => {
            // 1-6 days
            match days {
                1 => "yesterday".to_string(),
                _ => format!("{days}d ago"),
            }
        }
        604800..=3023999 => {
            // 1-4 weeks
            let weeks = days / 7;
            match weeks {
                1 => "1w ago".to_string(),
                _ => format!("{weeks}w ago"),
            }
        }
        _ => {
            let mut years = now.year() - dt.year();
            let mut months = now.month0() as i32 - dt.month0() as i32;
            if now.day() < dt.day() {
                months -= 1;
            }
            if months < 0 {
                years -= 1;
                months += 12;
            }
            match (years, months) {
                (0, 1) => "1m ago".to_string(),
                (0, months) => format!("{months}mo ago"),
                (1, _) => "last year".to_string(),
                (years, _) => format!("{years}y ago"),
            }
        }
    }
}

#[derive(Clone, Debug)]
struct GitHubRepo {
    owner: String,
    name: String,
}

impl GitHubRepo {
    pub fn new(owner: String, name: String) -> Self {
        Self { owner, name }
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl FromStr for GitHubRepo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('/').collect();

        if parts.len() != 2 {
            return Err("Repository must be in 'owner/repo' format".to_string());
        }

        let owner = parts[0].trim();
        let name = parts[1].trim();

        if owner.is_empty() {
            return Err("Owner name cannot be empty".to_string());
        }
        if name.is_empty() {
            return Err("Repository name cannot be empty".to_string());
        }

        Ok(GitHubRepo::new(owner.to_string(), name.to_string()))
    }
}

impl std::fmt::Display for GitHubRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

// async fn get_job_logs(
//     owner: &str,
//     repo: &str,
//     job_id: u64,
//     token: &str,
// ) -> Result<String, Box<dyn Error>> {
//     let client = reqwest::Client::new();
//     let url = format!(
//         "https://api.github.com/repos/{}/{}/actions/jobs/{}/logs",
//         owner, repo, job_id
//     );

//     let response = client
//         .get(&url)
//         .header("Authorization", format!("Bearer {}", token))
//         .header("Accept", "application/vnd.github.v3+json")
//         .header(
//             "User-Agent",
//             format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
//         )
//         .send()
//         .await?;

//     if !response.status().is_success() {
//         let status = response.status();
//         if status == 404 {
//             return Err("Logs not available yet".into());
//         }
//         let error_text = response.text().await.unwrap_or_default();
//         return Err(format!(
//             "GitHub API request failed with status {}: {}",
//             status, error_text
//         )
//         .into());
//     }

//     let logs = response.text().await?;
//     Ok(logs)
// }

// fn get_last_n_log_lines(logs: &str, n: usize) -> Vec<String> {
//     let lines: Vec<&str> = logs.lines().collect();
//     let start_index = if lines.len() > n { lines.len() - n } else { 0 };

//     lines[start_index..]
//         .iter()
//         .map(|line| strip_ansi_codes(line))
//         .filter(|line| !line.trim().is_empty()) // Filter out empty lines
//         .collect()
// }

// fn strip_ansi_codes(input: &str) -> String {
//     let mut result = String::new();
//     let mut chars = input.chars().peekable();

//     while let Some(ch) = chars.next() {
//         if ch == '\x1b' && chars.peek() == Some(&'[') {
//             chars.next(); // consume '['
//             while let Some(next_ch) = chars.next() {
//                 if next_ch.is_alphabetic() {
//                     break;
//                 }
//             }
//         } else {
//             result.push(ch);
//         }
//     }

//     result
// }
