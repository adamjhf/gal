use std::env;
use std::error::Error;
use std::fmt;
use std::process::Command;

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

#[derive(serde::Deserialize, Debug)]
struct Workflow {
    id: u64,
    name: String,
    head_branch: String,
    status: String,
    conclusion: Option<String>,
    html_url: String,
    created_at: String,
}

#[derive(serde::Deserialize, Debug)]
struct WorkflowsResponse {
    workflow_runs: Vec<Workflow>,
}

#[derive(serde::Deserialize, Debug)]
struct Job {
    // id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    html_url: String,
}

#[derive(serde::Deserialize, Debug)]
struct JobsResponse {
    jobs: Vec<Job>,
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
    run_id: u64,
    token: &str,
) -> Result<Vec<Job>, Box<dyn Error>> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs/{}/jobs",
        owner, repo, run_id
    );

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github.v3+json")
        .header(
            "User-Agent",
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!(
            "GitHub API request failed with status {}: {}",
            status, error_text
        )
        .into());
    }

    let jobs_response: JobsResponse = response.json().await?;
    Ok(jobs_response.jobs)
}

async fn get_workflows(
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<Vec<Workflow>, Box<dyn Error>> {
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/repos/{owner}/{repo}/actions/runs");

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github.v3+json")
        .header(
            "User-Agent",
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!("GitHub API request failed with status {status}: {error_text}").into());
    }

    let workflows_response: WorkflowsResponse = response.json().await?;
    Ok(workflows_response.workflow_runs)
}

fn get_status_symbol(status: &str, conclusion: &Option<String>) -> &'static str {
    match status {
        "in_progress" => "🔄",
        "queued" => "⏳",
        "completed" => match conclusion.as_ref().map(|s| s.as_str()) {
            Some("success") => "✅",
            Some("failure") => "❌",
            Some("cancelled") => "🚫",
            Some("skipped") => "⏭️",
            _ => "🔘",
        },
        _ => "❓",
    }
}

fn format_duration(started_at: &Option<String>, completed_at: &Option<String>) -> String {
    match (started_at, completed_at) {
        (Some(start), Some(end)) => {
            match (
                chrono::DateTime::parse_from_rfc3339(start),
                chrono::DateTime::parse_from_rfc3339(end),
            ) {
                (Ok(start_time), Ok(end_time)) => {
                    let duration = end_time.signed_duration_since(start_time);
                    let total_seconds = duration.num_seconds();
                    let minutes = total_seconds / 60;
                    let seconds = total_seconds % 60;
                    if minutes > 0 {
                        format!("{}m {}s", minutes, seconds)
                    } else {
                        format!("{}s", seconds)
                    }
                }
                _ => "Unknown duration".to_string(),
            }
        }
        (Some(_), None) => "Running...".to_string(),
        _ => "Not started".to_string(),
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
    let workflows = get_workflows(&owner, &repo, &token).await?;

    if workflows.is_empty() {
        println!("No currently running GitHub Actions found.");
    } else {
        println!("Found {} workflow(s):\n", workflows.len());

        for workflow in workflows {
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
                    if jobs.is_empty() {
                        println!("     No jobs found for this workflow.");
                    } else {
                        for job in jobs {
                            let emoji = get_status_symbol(&job.status, &job.conclusion);
                            let duration = format_duration(&job.started_at, &job.completed_at);

                            println!("     {} {} ({})", emoji, job.name, job.status);
                            println!("       Duration: {}", duration);
                            if let Some(conclusion) = job.conclusion {
                                println!("       Conclusion: {}", conclusion);
                            }
                            println!("       URL: {}", job.html_url);
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
