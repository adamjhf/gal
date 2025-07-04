# gal - GitHub Actions Live Monitor

A terminal-based GitHub Actions workflow monitor that provides near-real-time updates on your CI/CD pipelines.

![Image](https://github.com/user-attachments/assets/06d5eec3-871c-4c06-bf5e-bec3de02d727)

## Installation

### Cargo

```bash
cargo install gal-cli
```

## Usage

```bash
# Monitor current repository (auto-detected from git origin)
gal

# Monitor specific repository
gal --repo owner/repo

# Monitor specific branches only
gal --repo owner/repo --branch main,develop

# Enable file logging
gal --repo owner/repo --log /path/to/logfile.log
```

## Environment Variables

- `GITHUB_TOKEN` - GitHub personal access token (required for private repos and increased rate limits)

## Command Line Options

```
Options:
  -r, --repo <OWNER/REPO>    GitHub repository (defaults to current git repo)
  -b, --branch <BRANCH>      Filter to specific branches (comma-separated)
  -l, --log <FILE>           Output logs to a file
  -h, --help                 Print help
  -V, --version              Print version
```
## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
