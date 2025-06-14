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
    // Handle both SSH and HTTPS URLs
    let cleaned_url = if url.starts_with("git@github.com:") {
        // SSH format: git@github.com:owner/repo.git
        url.strip_prefix("git@github.com:")
            .ok_or("Invalid SSH URL format")?
    } else if url.starts_with("https://github.com/") {
        // HTTPS format: https://github.com/owner/repo.git
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

async fn get_running_workflows(
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<Vec<Workflow>, Box<dyn Error>> {
    let client = reqwest::Client::new();
    let url =
        format!("https://api.github.com/repos/{owner}/{repo}/actions/runs?status=in_progress");

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", "github-actions-checker")
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Get GitHub token from environment
    let token = env::var("GITHUB_TOKEN")
        .map_err(|_| AppError::from("GITHUB_TOKEN environment variable not set"))?;

    // Get git origin URL
    let origin_url = get_git_origin_url()?;
    println!("Git origin URL: {origin_url}");

    // Parse owner and repo from URL
    let (owner, repo) = parse_github_repo(&origin_url)?;
    println!("Repository: {owner}/{repo}");

    // Fetch running workflows
    println!("\nFetching running GitHub Actions...");
    let workflows = get_running_workflows(&owner, &repo, &token).await?;

    if workflows.is_empty() {
        println!("No currently running GitHub Actions found.");
    } else {
        println!("Found {} running workflow(s):\n", workflows.len());

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
            println!();
        }
    }

    Ok(())
}
