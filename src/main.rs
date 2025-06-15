use std::env;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::ErrReport, eyre::eyre};
use crossterm::event::{Event, EventStream, KeyCode};
use octocrab::{
    Octocrab, Page,
    models::{
        RunId,
        workflows::{Conclusion, Job, Run, Status},
    },
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Cell, HighlightSpacing, Row, StatefulWidget, Table, TableState, Widget,
};
use ratatui::{DefaultTerminal, Frame};
use tokio_stream::StreamExt;
use unicode_width::UnicodeWidthStr;

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

// async fn get_workflow_jobs(
//     client: &Octocrab,
//     owner: &str,
//     repo: &str,
//     run_id: RunId,
// ) -> Result<Page<Job>, Box<dyn Error>> {
//     let jobs = client
//         .workflows(owner, repo)
//         .list_jobs(run_id)
//         .send()
//         .await?;
//     Ok(jobs)
// }

async fn get_workflow_runs(client: &Octocrab, owner: &str, repo: &str) -> Result<Page<Run>> {
    let runs = client.workflows(owner, repo).list_all_runs().send().await?;
    Ok(runs)
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

fn get_status_symbol(status: &Status, conclusion: &Option<Conclusion>) -> &'static str {
    match status {
        Status::InProgress => "🔄",
        Status::Queued => "⏳",
        Status::Completed => match conclusion.as_ref() {
            Some(Conclusion::Success) => "✅",
            Some(Conclusion::Failure) => "❌",
            Some(Conclusion::Cancelled) => "🚫",
            Some(Conclusion::Skipped) => "⏭️",
            _ => "🔘",
        },
        _ => "❓",
    }
}

fn format_duration(started_at: &DateTime<Utc>, completed_at: &Option<DateTime<Utc>>) -> String {
    match completed_at {
        Some(end_time) => {
            let duration = end_time.signed_duration_since(started_at);
            let total_seconds = duration.num_seconds();
            let minutes = total_seconds / 60;
            let seconds = total_seconds % 60;
            if minutes > 0 {
                format!("{minutes}m {seconds}s")
            } else {
                format!("{seconds}s")
            }
        }
        None => "Running...".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let terminal = ratatui::init();
    let app_result = App::default().run(terminal).await;
    ratatui::restore();
    app_result
}

#[derive(Debug, Default)]
struct App {
    should_quit: bool,
    workflow_runs: WorkflowRunsListWidget,
}

impl App {
    const FRAMES_PER_SECOND: f32 = 60.0;

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
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct WorkflowRunsListWidget {
    state: Arc<RwLock<WorkflowRunsListState>>,
}

#[derive(Debug, Default)]
struct WorkflowRunsListState {
    workflow_runs: Vec<WorkflowRun>,
    constraint_lens: (u16, u16, u16, u16),
    loading_state: LoadingState,
    table_state: TableState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum LoadingState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Error(String),
}

fn constraint_lens(runs: &[WorkflowRun]) -> (u16, u16, u16, u16) {
    let status_len = runs
        .iter()
        .map(|run| run.status.as_str())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    let id_len = runs
        .iter()
        .map(|run| run.id.as_str())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    let name_len = runs
        .iter()
        .map(|run| run.name.as_str())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    let branch_len = runs
        .iter()
        .map(|run| run.branch.as_str())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    (
        id_len as u16,
        branch_len as u16,
        name_len as u16,
        status_len as u16,
    )
}

impl WorkflowRunsListWidget {
    fn run(&self) {
        let this = self.clone();
        tokio::spawn(this.fetch_runs());
    }

    async fn fetch_runs(self) {
        // this runs once, but you could also run this in a loop, using a channel that accepts
        // messages to refresh on demand, or with an interval timer to refresh every N seconds
        self.set_loading_state(LoadingState::Loading);
        match get_all_workflow_runs().await {
            Ok(page) => self.on_load(&page),
            Err(err) => self.on_err(&err),
        }
    }

    fn on_load(&self, page: &Page<Run>) {
        let runs = page.items.iter().map(Into::into);
        let mut state = self.state.write().unwrap();
        state.loading_state = LoadingState::Loaded;
        state.workflow_runs.extend(runs);
        state.constraint_lens = constraint_lens(&state.workflow_runs);
        if !state.workflow_runs.is_empty() {
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
}

async fn get_all_workflow_runs() -> Result<Page<Run>> {
    let token =
        env::var("GITHUB_TOKEN").map_err(|_| eyre!("GITHUB_TOKEN environment variable not set"))?;

    let origin_url = get_git_origin_url()?;

    let (owner, _repo) = parse_github_repo(&origin_url)?;
    let repo = "wayfarer";

    let octocrab = octocrab::Octocrab::builder()
        .personal_token(token.to_owned())
        .build()?;

    let workflows = get_workflow_runs(&octocrab, &owner, repo).await?;

    Ok(workflows)
}

type OctoRun = octocrab::models::workflows::Run;

impl From<&OctoRun> for WorkflowRun {
    fn from(run: &OctoRun) -> Self {
        Self {
            id: format!("{}", run.id.0),
            name: run.name.to_string(),
            branch: run.head_branch.to_string(),
            status: run.conclusion.as_ref().unwrap_or(&run.status).to_string(),
            created_at: run.created_at,
            html_url: run.html_url.as_ref().into(),
        }
    }
}

impl Widget for &WorkflowRunsListWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut state = self.state.write().unwrap();

        let loading_state = Line::from(format!("{:?}", state.loading_state)).right_aligned();
        let block = Block::bordered()
            .title("Workflow Runs")
            .title(loading_state)
            .title_bottom("j/k to scroll, q to quit");

        let header = ["ID", "Branch", "Name", "Status"]
            .into_iter()
            .map(Cell::from)
            .collect::<Row>()
            .style(Style::new().bold())
            .height(1);
        let rows = state.workflow_runs.iter();
        let widths = [
            Constraint::Length(state.constraint_lens.0 + 1),
            Constraint::Length(state.constraint_lens.1 + 1),
            Constraint::Fill(1),
            Constraint::Length(state.constraint_lens.3 + 1),
        ];
        let table = Table::new(rows, widths)
            .block(block)
            .header(header)
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol("> ")
            .row_highlight_style(Style::new().on_dark_gray());

        if state.loading_state != LoadingState::Loading {
            StatefulWidget::render(table, area, buf, &mut state.table_state);
        }
    }
}

#[derive(Debug, Clone)]
struct WorkflowRun {
    id: String,
    name: String,
    branch: String,
    status: String,
    created_at: DateTime<Utc>,
    html_url: String,
}

impl From<&WorkflowRun> for Row<'_> {
    fn from(runs: &WorkflowRun) -> Self {
        let run = runs.clone();
        Row::new(vec![run.id, run.branch, run.name, run.status])
    }
}

// #[tokio::main]
// async fn main() -> Result<(), Box<dyn Error>> {
//     let token = env::var("GITHUB_TOKEN")
//         .map_err(|_| AppError::from("GITHUB_TOKEN environment variable not set"))?;

//     let origin_url = get_git_origin_url()?;
//     println!("Git origin URL: {origin_url}");

//     let (owner, _repo) = parse_github_repo(&origin_url)?;
//     let repo = "wayfarer";
//     println!("Repository: {owner}/{repo}");

//     let octocrab = octocrab::Octocrab::builder()
//         .personal_token(token.to_owned())
//         .build()?;

//     println!("\nFetching running GitHub Actions...");
//     let workflows = get_workflow_runs(&octocrab, &owner, repo).await?;

//     if workflows.items.is_empty() {
//         println!("No currently running GitHub Actions found.");
//     } else {
//         println!("Found {} workflow(s):\n", workflows.items.len());

//         for workflow in workflows.items {
//             println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
//             println!("🔄 {} (ID: {})", workflow.name, workflow.id);
//             println!("   Branch: {}", workflow.head_branch);
//             println!("   Status: {}", workflow.status);
//             if let Some(conclusion) = workflow.conclusion {
//                 println!("   Conclusion: {conclusion}");
//             }
//             println!("   Started: {}", workflow.created_at);
//             println!("   URL: {}", workflow.html_url);
//             println!("\n   Jobs:");
//             match get_workflow_jobs(&octocrab, &owner, repo, workflow.id).await {
//                 Ok(jobs) => {
//                     if jobs.items.is_empty() {
//                         println!("     No jobs found for this workflow.");
//                     } else {
//                         for job in jobs.items {
//                             let emoji = get_status_symbol(&job.status, &job.conclusion);
//                             let duration = format_duration(&job.started_at, &job.completed_at);

//                             println!("     {} {} ({:?})", emoji, job.name, job.status);
//                             println!("       Duration: {duration}");
//                             if let Some(conclusion) = job.conclusion {
//                                 println!("       Conclusion: {conclusion:?}");
//                             }
//                             println!("       URL: {}", job.html_url);
//                             // if job.status == "in_progress" {
//                             //     match get_job_logs(&owner, &repo, job.id, &token).await {
//                             //         Ok(logs) => {
//                             //             let last_lines = get_last_n_log_lines(&logs, 5);
//                             //             if last_lines.is_empty() {
//                             //                 println!("         No recent log output available");
//                             //             } else {
//                             //                 for line in last_lines {
//                             //                     if !line.trim().is_empty() {
//                             //                         let display_line = if line.len() > 100 {
//                             //                             format!("{}...", &line[..97])
//                             //                         } else {
//                             //                             line
//                             //                         };
//                             //                         println!("         │ {}", display_line);
//                             //                     }
//                             //                 }
//                             //             }
//                             //         }
//                             //         Err(e) => {
//                             //             println!("         Error fetching logs: {}", e);
//                             //         }
//                             //     }
//                             // }
//                             println!();
//                         }
//                     }
//                 }
//                 Err(e) => {
//                     println!("     Error fetching jobs: {e}");
//                 }
//             }
//             println!();
//         }
//     }

//     Ok(())
// }
