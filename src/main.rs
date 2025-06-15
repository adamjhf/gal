use std::env;
use std::error::Error;
use std::fmt;
use std::process::Command;

use chrono::{DateTime, Utc};
use octocrab::{
    Page,
    models::{
        RunId,
        workflows::{Conclusion, Job, Run, Status},
    },
};

#[derive(Debug)]
struct AppError {
    message: String,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for AppError {}

impl From<&str> for AppError {
    fn from(msg: &str) -> Self {
        AppError {
            message: msg.to_string(),
        }
    }
}

fn get_git_origin_url() -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()?;

    if !output.status.success() {
        return Err("Failed to get git origin URL. Make sure you're in a git repository.".into());
    }

    let url = String::from_utf8(output.stdout)?.trim().to_string();

    if url.is_empty() {
        return Err("No origin URL found. Make sure the repository has a remote origin.".into());
    }

    Ok(url)
}

fn parse_github_repo(url: &str) -> Result<(String, String), Box<dyn Error>> {
    let cleaned_url = if url.starts_with("git@github.com:") {
        url.strip_prefix("git@github.com:")
            .ok_or("Invalid SSH URL format")?
    } else if url.starts_with("https://github.com/") {
        url.strip_prefix("https://github.com/")
            .ok_or("Invalid HTTPS URL format")?
    } else {
        return Err("URL must be a GitHub repository (SSH or HTTPS)".into());
    };

    // Remove .git suffix if present
    let repo_path = cleaned_url.strip_suffix(".git").unwrap_or(cleaned_url);

    // Split into owner and repo
    let parts: Vec<&str> = repo_path.split('/').collect();
    if parts.len() != 2 {
        return Err("Invalid repository path format. Expected owner/repo".into());
    }

    Ok((parts[0].to_string(), parts[1].to_string()))
}

async fn get_workflow_jobs(
    owner: &str,
    repo: &str,
    run_id: RunId,
    token: &str,
) -> Result<Page<Job>, Box<dyn Error>> {
    let octocrab = octocrab::Octocrab::builder()
        .personal_token(token.to_owned())
        .build()?;
    let jobs = octocrab
        .workflows(owner, repo)
        .list_jobs(run_id)
        .send()
        .await?;
    Ok(jobs)
}

async fn get_workflow_runs(
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<Page<Run>, Box<dyn Error>> {
    let octocrab = octocrab::Octocrab::builder()
        .personal_token(token.to_owned())
        .build()?;
    let runs = octocrab
        .workflows(owner, repo)
        .list_all_runs()
        .send()
        .await?;
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
                format!("{}m {}s", minutes, seconds)
            } else {
                format!("{}s", seconds)
            }
        }
        None => "Running...".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let token = env::var("GITHUB_TOKEN")
        .map_err(|_| AppError::from("GITHUB_TOKEN environment variable not set"))?;

    let origin_url = get_git_origin_url()?;
    println!("Git origin URL: {origin_url}");

    let (owner, _repo) = parse_github_repo(&origin_url)?;
    let repo = "wayfarer";
    println!("Repository: {owner}/{repo}");

    println!("\nFetching running GitHub Actions...");
    let workflows = get_workflow_runs(&owner, &repo, &token).await?;

    if workflows.items.is_empty() {
        println!("No currently running GitHub Actions found.");
    } else {
        println!("Found {} workflow(s):\n", workflows.items.len());

        for workflow in workflows.items {
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("🔄 {} (ID: {})", workflow.name, workflow.id);
            println!("   Branch: {}", workflow.head_branch);
            println!("   Status: {}", workflow.status);
            if let Some(conclusion) = workflow.conclusion {
                println!("   Conclusion: {conclusion}");
            }
            println!("   Started: {}", workflow.created_at);
            println!("   URL: {}", workflow.html_url);
            println!("\n   Jobs:");
            match get_workflow_jobs(&owner, &repo, workflow.id, &token).await {
                Ok(jobs) => {
                    if jobs.items.is_empty() {
                        println!("     No jobs found for this workflow.");
                    } else {
                        for job in jobs.items {
                            let emoji = get_status_symbol(&job.status, &job.conclusion);
                            let duration = format_duration(&job.started_at, &job.completed_at);

                            println!("     {} {} ({:?})", emoji, job.name, job.status);
                            println!("       Duration: {}", duration);
                            if let Some(conclusion) = job.conclusion {
                                println!("       Conclusion: {:?}", conclusion);
                            }
                            println!("       URL: {}", job.html_url);
                            // if job.status == "in_progress" {
                            //     match get_job_logs(&owner, &repo, job.id, &token).await {
                            //         Ok(logs) => {
                            //             let last_lines = get_last_n_log_lines(&logs, 5);
                            //             if last_lines.is_empty() {
                            //                 println!("         No recent log output available");
                            //             } else {
                            //                 for line in last_lines {
                            //                     if !line.trim().is_empty() {
                            //                         let display_line = if line.len() > 100 {
                            //                             format!("{}...", &line[..97])
                            //                         } else {
                            //                             line
                            //                         };
                            //                         println!("         │ {}", display_line);
                            //                     }
                            //                 }
                            //             }
                            //         }
                            //         Err(e) => {
                            //             println!("         Error fetching logs: {}", e);
                            //         }
                            //     }
                            // }
                            println!();
                        }
                    }
                }
                Err(e) => {
                    println!("     Error fetching jobs: {}", e);
                }
            }
            println!();
        }
    }

    Ok(())
}
