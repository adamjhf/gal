use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::{cmp, env};

use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::ErrReport, eyre::eyre};
use crossterm::event::{Event, EventStream, KeyCode};
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
use tokio::time::interval;
use tokio_stream::StreamExt;
use tracing::{error, info};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use unicode_width::UnicodeWidthStr;

#[tokio::main]
async fn main() -> Result<()> {
    let file_appender = RollingFileAppender::new(Rotation::DAILY, "logs", "app.log");
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer().with_writer(file_appender))
        .init();

    color_eyre::install()?;
    let terminal = ratatui::init();
    let app_result = App::new().run(terminal).await;
    ratatui::restore();
    app_result
}

#[derive(Debug)]
struct App {
    workflow_runs: WorkflowRunsListWidget,
    should_quit: bool,
}

impl App {
    const FRAMES_PER_SECOND: f32 = 60.0;

    pub fn new() -> Self {
        let origin_url = get_git_origin_url().unwrap();
        let (owner, _repo) = parse_github_repo(&origin_url).unwrap();
        let repo = "nixos".to_string();

        let token = env::var("GITHUB_TOKEN").unwrap();
        let octocrab = octocrab::Octocrab::builder()
            .personal_token(token.to_owned())
            .build()
            .unwrap();

        let workflow_runs = WorkflowRunsListWidget::new(octocrab, owner, repo);

        Self {
            workflow_runs,
            should_quit: false,
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> color_eyre::Result<()> {
        self.workflow_runs.run();

        let period = Duration::from_secs_f32(1.0 / Self::FRAMES_PER_SECOND);
        let mut interval = tokio::time::interval(period);
        let mut events = EventStream::new();

        while !self.should_quit {
            tokio::select! {
                _ = interval.tick() => { terminal.draw(|frame| self.render(frame))?; },
                Some(Ok(event)) = events.next() => self.handle_event(&event),
            }
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame) {
        let vertical = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]);
        let [title_area, body_area] = vertical.areas(frame.area());
        let title = Line::from("GitHub Actions Listener").centered().bold();
        frame.render_widget(title, title_area);
        frame.render_widget(&self.workflow_runs, body_area);
    }

    fn handle_event(&mut self, event: &Event) {
        if let Some(key) = event.as_key_press_event() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                KeyCode::Char('j') | KeyCode::Down => self.workflow_runs.scroll_down(),
                KeyCode::Char('k') | KeyCode::Up => self.workflow_runs.scroll_up(),
                KeyCode::Enter => self.workflow_runs.open_selected_workflow(),
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
struct WorkflowRunsListWidget {
    state: Arc<RwLock<WorkflowRunsListState>>,
    client: Octocrab,
    repo_owner: String,
    repo: String,
}

#[derive(Debug, Default)]
struct WorkflowRunsListState {
    workflow_runs: Vec<WorkflowRun>,
    constraint_lens: (u16, u16, u16),
    loading_state: LoadingState,
    table_state: TableState,
    completed_workflow_jobs: HashMap<u64, Vec<Job>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum LoadingState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Error(String),
}

impl WorkflowRunsListWidget {
    fn new(client: Octocrab, repo_owner: String, repo: String) -> Self {
        Self {
            state: Default::default(),
            client,
            repo_owner,
            repo,
        }
    }

    fn run(&self) {
        let this = self.clone();
        tokio::spawn(this.fetch_runs());
    }

    async fn fetch_runs(self) {
        let mut interval = interval(Duration::from_secs(10));
        loop {
            self.set_loading_state(LoadingState::Loading);
            match self.get_detailed_workflow_runs().await {
                Ok(runs) => self.on_load(runs),
                Err(err) => self.on_err(&err),
            }
            interval.tick().await;
        }
    }

    fn on_load(&self, runs: Vec<WorkflowRun>) {
        let mut state = self.state.write().unwrap();
        state.loading_state = LoadingState::Loaded;
        let was_empty = state.workflow_runs.is_empty();
        state.workflow_runs = runs;
        state.constraint_lens = constraint_lens(&state.workflow_runs);
        if !state.workflow_runs.is_empty() && was_empty {
            state.table_state.select(Some(0));
        }
    }

    fn on_err(&self, err: &ErrReport) {
        self.set_loading_state(LoadingState::Error(err.to_string()));
    }

    fn set_loading_state(&self, state: LoadingState) {
        self.state.write().unwrap().loading_state = state;
    }

    fn scroll_down(&self) {
        self.state.write().unwrap().table_state.scroll_down_by(1);
    }

    fn scroll_up(&self) {
        self.state.write().unwrap().table_state.scroll_up_by(1);
    }

    async fn get_detailed_workflow_runs(&self) -> Result<Vec<WorkflowRun>> {
        let workflows = match self.get_workflow_runs().await {
            Ok(runs) => runs,
            Err(err) => {
                error!("{:?}", err);
                return Err(err);
            }
        };

        let details_futures = workflows.into_iter().map(|run| async move {
            let status = run.conclusion.as_ref().unwrap_or(&run.status);
            let job_statuses = match status.as_str() {
                "failure" | "in_progress" => {
                    match self
                        .get_workflow_jobs(run.id, run.status == "completed")
                        .await
                    {
                        Ok(jobs) => {
                            let mut all_items = Vec::new();

                            for (job_index, job) in jobs.iter().enumerate() {
                                let is_last_job = job_index == jobs.len() - 1;
                                let job_prefix = if is_last_job { "└─ " } else { "├─ " };

                                let job_status =
                                    get_job_status_symbol(&job.status, &job.conclusion);
                                all_items.push(ColoredText::new(
                                    job_prefix.into(),
                                    format!("{} {}", job_status.symbol, job.name),
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
                                    } else {
                                        if is_last_step {
                                            "│  └─ "
                                        } else {
                                            "│  ├─ "
                                        }
                                    };

                                    let status =
                                        get_job_status_symbol(&step.status, &step.conclusion);
                                    all_items.push(ColoredText::new(
                                        step_prefix.into(),
                                        format!("{} {}", status.symbol, step.name),
                                        status.color,
                                    ));
                                }
                            }

                            all_items
                        }
                        Err(err) => {
                            error!("{:?}", err);
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            };
            let run_status = get_run_status_symbol(&run.status, &run.conclusion);
            WorkflowRun {
                id: format!("{}", run.id.clone().0),
                branch: run.head_branch.to_string(),
                details: vec![ColoredText::new(
                    String::new(),
                    format!("{} {}", run_status.symbol, run.name,),
                    run_status.color,
                )]
                .into_iter()
                .chain(job_statuses)
                .collect(),
                created_at: run.created_at,
                html_url: run.html_url.clone().into(),
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
                info!("fetching jobs for workflow run {}", run_id.0);
                let jobs = self
                    .client
                    .workflows(&self.repo_owner, &self.repo)
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
        info!("fetching workflow runs");
        let runs = self
            .client
            .workflows(&self.repo_owner, &self.repo)
            .list_all_runs()
            .send()
            .await?
            .items;
        Ok(runs)
    }

    fn open_selected_workflow(&self) {
        let state = self.state.read().unwrap();
        if let Some(selected_index) = state.table_state.selected() {
            if let Some(workflow_run) = state.workflow_runs.get(selected_index) {
                let url = &workflow_run.html_url;
                info!("Opening URL: {}", url);

                if let Err(e) = webbrowser::open(url) {
                    error!("Failed to open browser: {}", e);
                }
            }
        }
    }
}

impl Widget for &WorkflowRunsListWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut state = self.state.write().unwrap();

        let loading_state = Line::from(format!("{:?}", state.loading_state)).right_aligned();
        let block = Block::bordered()
            .title(format!("{}/{}", self.repo_owner, self.repo))
            .title(loading_state)
            .title_bottom("j/k to scroll, enter to open in browser, q to quit");

        if state.workflow_runs.is_empty() {
            let loading_message = match &state.loading_state {
                LoadingState::Loading => "Loading workflow runs from GitHub...",
                LoadingState::Error(err) => err.as_str(),
                LoadingState::Idle => "No workflow runs found.",
                LoadingState::Loaded => "No workflow runs available.",
            };

            let paragraph = Paragraph::new(Text::from(loading_message))
                .block(block)
                .alignment(Alignment::Center)
                .wrap(ratatui::widgets::Wrap { trim: true });

            paragraph.render(area, buf);
        } else {
            let header = ["ID", "Branch", "Run"]
                .into_iter()
                .map(Cell::from)
                .collect::<Row>()
                .style(Style::new().bold())
                .height(1);
            let rows = state.workflow_runs.iter();
            let widths = [
                Constraint::Length(state.constraint_lens.0 + 1),
                Constraint::Length(cmp::max(state.constraint_lens.1 + 1, 7)),
                Constraint::Fill(1),
            ];
            let table = Table::new(rows, widths)
                .block(block)
                .header(header)
                .highlight_spacing(HighlightSpacing::Always)
                .highlight_symbol("> ")
                .row_highlight_style(Style::new().on_dark_gray().bold());

            StatefulWidget::render(table, area, buf, &mut state.table_state);
        }
    }
}

#[derive(Debug, Clone)]
struct ColoredText {
    prefix: String,
    text: String,
    color: Color,
}

impl ColoredText {
    fn new(prefix: String, text: String, color: Color) -> Self {
        Self {
            prefix,
            text,
            color,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct WorkflowRun {
    id: String,
    branch: String,
    details: Vec<ColoredText>,
    created_at: DateTime<Utc>,
    html_url: String,
}

impl From<&WorkflowRun> for Row<'_> {
    fn from(runs: &WorkflowRun) -> Self {
        let run = runs.clone();
        Row::new(vec![
            Cell::from(run.id),
            Cell::from(run.branch),
            Cell::from(
                run.details
                    .into_iter()
                    .map(|ct| {
                        Line::from(vec![
                            Span::styled(ct.prefix, Style::default().fg(Color::DarkGray)),
                            Span::styled(ct.text, Style::default().fg(ct.color)),
                        ])
                    })
                    .collect::<Vec<_>>(),
            ),
        ])
        .height(runs.details.len() as u16)
    }
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

fn get_run_status_symbol(status: &String, conclusion: &Option<String>) -> StatusDisplay {
    match status.as_str() {
        "in_progress" => ("⏵", Color::Yellow),
        "queued" => ("⏸", Color::Blue),
        "completed" => match conclusion.as_ref().map(|s| s.as_str()) {
            Some("success") => ("✔", Color::Green),
            Some("failure") => ("⚠", Color::Red),
            Some("cancelled") => ("∅", Color::Gray),
            Some("skipped") => ("⏭", Color::Magenta),
            _ => ("⏺", Color::Green),
        },
        _ => ("?", Color::Magenta),
    }
    .into()
}

fn get_job_status_symbol(status: &Status, conclusion: &Option<Conclusion>) -> StatusDisplay {
    match status {
        Status::InProgress => ("⏵", Color::Yellow),
        Status::Queued => ("⏸", Color::Blue),
        Status::Completed => match conclusion.as_ref() {
            Some(Conclusion::Success) => ("✔", Color::Green),
            Some(Conclusion::Failure) => ("⚠", Color::Red),
            Some(Conclusion::Cancelled) => ("∅", Color::Gray),
            Some(Conclusion::Skipped) => ("⏭", Color::Magenta),
            _ => ("⏺", Color::Green),
        },
        _ => ("?", Color::Magenta),
    }
    .into()
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

fn constraint_lens(runs: &[WorkflowRun]) -> (u16, u16, u16) {
    let details_len = runs
        .iter()
        .flat_map(|run| run.details.iter().map(|s| s.text.as_str()))
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    let id_len = runs
        .iter()
        .map(|run| run.id.as_str())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    let branch_len = runs
        .iter()
        .map(|run| run.branch.as_str())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    (id_len as u16, branch_len as u16, details_len as u16)
}

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

fn parse_github_repo(url: &str) -> Result<(String, String)> {
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

    Ok((parts[0].to_string(), parts[1].to_string()))
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
