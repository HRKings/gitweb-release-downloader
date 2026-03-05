mod arguments;
mod models;
use std::{
    fs::{self, File},
    io::{stderr, Write},
    net::ToSocketAddrs,
    path::PathBuf,
    process::{self, exit},
    thread,
    time::{Duration, SystemTime},
};

use arguments::{GitWebsite, IpType};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use models::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use ureq::{Agent, Resolver, Response};

impl Resolver for IpType {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        ToSocketAddrs::to_socket_addrs(netloc).map(|iter| {
            iter.filter(|address| match self {
                Self::Any => true,
                Self::IPV4 => address.is_ipv4(),
                Self::IPV6 => address.is_ipv6(),
            })
            .collect()
        })
    }
}

fn get_default_agent(ip_type: IpType) -> Agent {
    ureq::AgentBuilder::new().resolver(ip_type).build()
}

// GitHub requires the usage of a user agent
const USERAGENT: &str = "gitweb-release-downloader";

const CACHE_TTL_SECS: u64 = 3600;

#[derive(Serialize, Deserialize)]
struct CachedReleases {
    cached_at: u64,
    releases: Vec<Release>,
}

fn get_cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
            PathBuf::from(home).join(".cache")
        })
        .join("grd")
}

fn get_cache_path(repository: &arguments::Repository) -> PathBuf {
    get_cache_dir()
        .join(&repository.origin)
        .join(&repository.owner)
        .join(format!("{}.json", &repository.name))
}

fn read_cache(path: &PathBuf) -> Option<Vec<Release>> {
    let data = fs::read_to_string(path).ok()?;
    let cached: CachedReleases = serde_json::from_str(&data).ok()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if now.saturating_sub(cached.cached_at) < CACHE_TTL_SECS {
        Some(cached.releases)
    } else {
        None
    }
}

fn write_cache(path: &PathBuf, releases: &[Release]) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cached = CachedReleases {
        cached_at: now,
        releases: releases.to_vec(),
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, serde_json::to_string(&cached).unwrap_or_default());
}

fn find_release<'a>(
    releases: &'a [Release],
    tag: Option<&str>,
    allow_prerelease: bool,
) -> Option<&'a Release> {
    for release in releases {
        if release.prerelease && !allow_prerelease {
            continue;
        }
        // if tag is latest take the first, which is
        // the latest
        match tag {
            None => return Some(release),
            Some(tag) => {
                if release.tag_name == tag {
                    return Some(release);
                }
            }
        }
    }
    None
}

fn find_asset<'a>(
    releases: &'a [Release],
    tag: Option<&str>,
    allow_prerelease: bool,
    asset_name_pattern: &Regex,
) -> Option<&'a Asset> {
    let release = find_release(releases, tag, allow_prerelease)?;
    release
        .assets
        .iter()
        .find(|&asset| asset_name_pattern.is_match(&asset.name))
}

fn find_assets_in_release<'a>(release: &'a Release, asset_name_pattern: &Regex) -> Vec<&'a Asset> {
    let mut matching_assets = vec![];
    for asset in &release.assets {
        if asset_name_pattern.is_match(&asset.name) {
            matching_assets.push(asset);
        }
    }
    matching_assets
}

#[inline(always)]
fn get_scheme_from_repository_string(url: &str) -> &str {
    if url.starts_with("http://") {
        "http"
    } else {
        "https"
    }
}

fn get_releases_api_url(repository: &arguments::Repository) -> String {
    let scheme = get_scheme_from_repository_string(&repository.passed_string);
    match repository.website {
        arguments::GitWebsite::GitHub => {
            format!(
                "{scheme}://api.github.com/repos/{owner}/{name}/releases",
                owner = repository.owner,
                name = repository.name,
            )
        }
        arguments::GitWebsite::Gitea => format!(
            "{scheme}://{origin}{sub_path}api/v1/repos/{owner}/{name}/releases",
            origin = repository.origin,
            sub_path = repository.sub_path,
            owner = repository.owner,
            name = repository.name
        ),
        arguments::GitWebsite::GitLab => format!(
            "{scheme}://{origin}{sub_path}api/v4/projects/{owner}%2F{name}/releases",
            origin = repository.origin,
            sub_path = repository.sub_path,
            owner = repository.owner,
            name = repository.name
        ),
    }
}

fn get_platform_headers(website: &GitWebsite) -> Vec<String> {
    match website {
        GitWebsite::GitHub => vec!["X-GitHub-Api-Version: 2022-11-28".to_string()],
        _ => vec![],
    }
}

fn get_releases(
    agent: &Agent,
    repository: &arguments::Repository,
    headers: &[String],
    force_refresh: bool,
) -> Vec<Release> {
    let cache_path = get_cache_path(repository);

    if !force_refresh {
        if let Some(cached) = read_cache(&cache_path) {
            return cached;
        }
    }

    let base_url = get_releases_api_url(repository);
    let mut all_releases: Vec<Release> = Vec::new();
    let mut page: u32 = 1;

    let page_size = match repository.website {
        GitWebsite::GitHub | GitWebsite::GitLab => 100,
        GitWebsite::Gitea => 50,
    };

    let page_size_param = match repository.website {
        GitWebsite::GitHub | GitWebsite::GitLab => "per_page",
        GitWebsite::Gitea => "limit",
    };

    let platform_headers = get_platform_headers(&repository.website);
    let all_headers: Vec<String> = platform_headers
        .iter()
        .chain(headers.iter())
        .cloned()
        .collect();

    loop {
        let separator = if base_url.contains('?') { '&' } else { '?' };
        let url = format!("{base_url}{separator}{page_size_param}={page_size}&page={page}");

        let response = make_get_request(agent, &url, &all_headers).unwrap_or_else(|e| {
            eprintln!("HTTP request failed:\n{e}");
            process::exit(1);
        });

        let json_string = response.into_string().unwrap_or_else(|e| {
            eprintln!("Could not get json from response:\n{e}");
            process::exit(1);
        });

        let page_releases: Vec<Release> = match repository.website {
            GitWebsite::GitHub | GitWebsite::Gitea => {
                serde_json::from_str(&json_string)
            }
            GitWebsite::GitLab => {
                serde_json::from_str::<Vec<GitLabRelease>>(&json_string)
                    .map(|e| e.into_iter().map(Into::into).collect())
            }
        }
        .unwrap_or_else(|e| {
            eprintln!("Could not deserialize json:\n{e}");
            process::exit(1);
        });

        if page_releases.is_empty() {
            break;
        }

        all_releases.extend(page_releases);
        page += 1;
    }

    write_cache(&cache_path, &all_releases);

    all_releases
}

fn get_compiled_asset_pattern_or_exit(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| {
        eprintln!("Could not compile RegEx:\n{e}");
        process::exit(1);
    })
}

fn get_asset_or_exit<'a>(
    releases: &'a [Release],
    parsed_args: &arguments::DownloadArgs,
    compiled_asset_pattern: &Regex,
) -> &'a Asset {
    let asset_option = find_asset(
        releases,
        parsed_args.tag.as_deref(),
        parsed_args.allow_prerelease,
        compiled_asset_pattern,
    );

    let Some(asset) = asset_option else {
        let tag_string = match &parsed_args.tag {
            Some(tag) => format!("tag \"{tag}\""),
            None => "latest tag".to_string(),
        };
        eprintln!(
            r#"Could not find Pattern "{asset_pattern}" in {tag_string} in releases of repository "{repository}""#,
            asset_pattern = parsed_args.asset_pattern,
            repository = parsed_args.repository.passed_string,
        );
        process::exit(1);
    };

    asset
}

fn make_get_request(
    agent: &Agent,
    url: &str,
    headers: &[String],
) -> Result<Response, Box<ureq::Error>> {
    let mut request = agent.get(url).set("user-agent", USERAGENT);
    for header in headers {
        // according to the first paragraph of the following mdn site, whitespace before the value
        // is ignored, so we don't need to remove anything
        // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers
        let (header_name, value) = header.split_once(":").unwrap_or_else(|| {
            eprintln!("Http header \"{header}\" has invalid format, must be: \"header-name: header-value\"");
            process::exit(1);
        });
        request = request.set(header_name, value);
    }

    request.call().map_err(Box::new)
}

fn make_get_request_with_retry(
    agent: &Agent,
    url: &str,
    headers: &[String],
    max_retries: u32,
) -> Result<Response, Box<ureq::Error>> {
    for attempt in 0..=max_retries {
        match make_get_request(agent, url, headers) {
            Ok(response) => return Ok(response),
            Err(e) => {
                let should_retry = matches!(*e, ureq::Error::Status(429, _) | ureq::Error::Status(403, _));
                if should_retry && attempt < max_retries {
                    let delay = if let ureq::Error::Status(_, ref resp) = *e {
                        resp.header("retry-after")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or_else(|| (1u64 << attempt).min(60))
                    } else {
                        (1u64 << attempt).min(60)
                    };
                    eprintln!(
                        "Rate limited, retrying in {delay}s... (attempt {}/{})",
                        attempt + 1,
                        max_retries
                    );
                    thread::sleep(Duration::from_secs(delay));
                    continue;
                }
                return Err(e);
            }
        }
    }
    unreachable!()
}

fn get_content_length(response: &Response) -> Option<usize> {
    response
        .header("content-length")
        .map_or_else(|| None, |input| input.parse::<usize>().ok())
}

fn create_progress_bar(content_length: usize) -> ProgressBar {
    let pb = ProgressBar::new(content_length as u64);
    let pb_style = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.green/red}] {bytes}/{total_bytes}",
    )
    // this hard coded template will always succeed compiling,
    // so it's okay to unwrap
    .unwrap()
    .progress_chars("=>-");
    pb.set_style(pb_style);
    pb
}

fn create_and_init_progress_bar(content_length_option: Option<usize>) -> Option<ProgressBar> {
    let content_length = content_length_option?;
    let pb = create_progress_bar(content_length);
    pb.set_position(0);
    Some(pb)
}

fn stream_response_into_file(
    response: Response,
    mut out_file: File,
    pb_option: &Option<ProgressBar>,
) {
    let mut stream = response.into_reader();

    let mut bytes_downloaded = 0;
    let mut buffer = [0_u8; 8192];

    let mut stderr_locked = stderr().lock();

    loop {
        let chunk_result = stream.read(&mut buffer);
        match chunk_result {
            Err(error) => {
                // can we even properly handle the potential error
                // of writeln! ?
                // If it fails we can't notify the user anyway
                writeln!(stderr_locked, "Error reading stream:\n{error}").unwrap();
                process::exit(1);
            }
            Ok(read_size) => {
                // download has finished
                if read_size == 0 {
                    break;
                }
                let file_write_result = out_file.write(&buffer[0..read_size]);
                if let Err(error) = file_write_result {
                    writeln!(stderr_locked, "Could not write to file:\n{error}").unwrap();
                    process::exit(1);
                }

                bytes_downloaded += read_size;

                if let Some(ref pb) = pb_option {
                    pb.set_position(bytes_downloaded as u64);
                }
            }
        }
    }
}

fn print_releases(releases_query_args: arguments::ReleasesQueryArgs) {
    let agent: Agent = get_default_agent(releases_query_args.connection_settings.ip_type);

    let repository: arguments::Repository = releases_query_args.repository;
    let releases = get_releases(
        &agent,
        &repository,
        &releases_query_args.connection_settings.headers,
        releases_query_args.connection_settings.force_refresh,
    );
    let releases_iter = releases
        .iter()
        .filter(|release| !release.prerelease || releases_query_args.allow_prerelease)
        .take(releases_query_args.count.into());
    for release in releases_iter {
        println!("{}", release.tag_name);
    }
}

fn print_assets(assets_query_args: arguments::AssetsQueryArgs) {
    let agent: Agent = get_default_agent(assets_query_args.connection_settings.ip_type);

    let releases = get_releases(
        &agent,
        &assets_query_args.repository,
        &assets_query_args.connection_settings.headers,
        assets_query_args.connection_settings.force_refresh,
    );
    // if no tag is specified, prereleases are not allowed
    // however if a tag is specified, the user explictly chose
    // a tag that might be a prerelease, so in this case it
    // will be allowed
    let allow_prerelease = assets_query_args.tag.is_some();
    let Some(release) = find_release(
        &releases,
        assets_query_args.tag.as_deref(),
        allow_prerelease,
    ) else {
        match &assets_query_args.tag {
            Some(tag) => eprintln!("Could not find release with tag \"{tag}\""),
            None => eprintln!("Could not find latest tag"),
        }
        process::exit(1);
    };
    let regex = get_compiled_asset_pattern_or_exit(&assets_query_args.pattern);
    let assets = find_assets_in_release(release, &regex);
    for asset in assets {
        println!("{}", asset.name);
    }
}

fn get_github_asset_api_url(owner: &str, repository: &str, asset_id: i64) -> String {
    format!("https://api.github.com/repos/{owner}/{repository}/releases/assets/{asset_id}")
}

fn download_all_assets(download_all_args: arguments::DownloadAllArgs) {
    let compiled_release_pattern =
        get_compiled_asset_pattern_or_exit(&download_all_args.release_pattern);
    let compiled_asset_pattern =
        get_compiled_asset_pattern_or_exit(&download_all_args.asset_pattern);

    let repository = &download_all_args.repository;
    let agent: Agent = get_default_agent(download_all_args.connection_settings.ip_type);
    let releases = get_releases(
        &agent,
        repository,
        &download_all_args.connection_settings.headers,
        download_all_args.connection_settings.force_refresh,
    );

    let matching_releases: Vec<&Release> = releases
        .iter()
        .filter(|r| !r.prerelease || download_all_args.allow_prerelease)
        .filter(|r| compiled_release_pattern.is_match(&r.tag_name))
        .collect();

    if matching_releases.is_empty() {
        eprintln!("No releases matched the given filters");
        process::exit(1);
    }

    let mut total_assets_downloaded: usize = 0;
    let mut releases_with_downloads: usize = 0;

    for release in &matching_releases {
        let assets = find_assets_in_release(release, &compiled_asset_pattern);
        if assets.is_empty() {
            continue;
        }

        let mut release_had_download = false;

        for asset in &assets {
            let out_path = download_all_args
                .output_dir
                .join(&release.tag_name)
                .join(&asset.name);

            if out_path.exists() && !download_all_args.overwrite {
                eprintln!("Skipping \"{}\" (already exists)", out_path.display());
                continue;
            }

            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).unwrap_or_else(|e| {
                    eprintln!("Error creating directory \"{}\":\n{e}", parent.display());
                    process::exit(1);
                });
            }

            let url: String;
            let mut headers = download_all_args.connection_settings.headers.clone();
            if matches!(repository.website, GitWebsite::GitHub) {
                headers.push("Accept: application/octet-stream".to_string());
                headers.push("X-GitHub-Api-Version: 2022-11-28".to_string());
                url = get_github_asset_api_url(&repository.owner, &repository.name, asset.id);
            } else {
                url = asset.browser_download_url.clone();
            }

            eprintln!("Downloading \"{}\"", out_path.display());

            let response =
                make_get_request_with_retry(&agent, &url, &headers, 5).unwrap_or_else(|e| {
                    eprintln!("Error downloading file:\n{e}");
                    process::exit(1);
                });

            let out_file = File::create(&out_path).unwrap_or_else(|e| {
                eprintln!("Error creating file \"{}\":\n{e}", out_path.display());
                process::exit(1);
            });

            let content_length_option = get_content_length(&response);
            let pb_option = create_and_init_progress_bar(content_length_option);

            stream_response_into_file(response, out_file, &pb_option);

            if let Some(ref pb) = pb_option {
                pb.finish();
                eprintln!();
            }

            thread::sleep(Duration::from_millis(200));

            if download_all_args.print_filenames {
                println!("{}", out_path.display());
            }

            total_assets_downloaded += 1;
            release_had_download = true;
        }

        if release_had_download {
            releases_with_downloads += 1;
        }
    }

    if total_assets_downloaded == 0 {
        eprintln!("No assets were downloaded");
        process::exit(1);
    }

    eprintln!(
        "Downloaded {} asset(s) across {} release(s)",
        total_assets_downloaded, releases_with_downloads
    );
}

fn download_assets(mut download_args: arguments::DownloadArgs) {
    let compiled_asset_pattern = get_compiled_asset_pattern_or_exit(&download_args.asset_pattern);

    let repository = &download_args.repository;
    let agent: Agent = get_default_agent(download_args.connection_settings.ip_type);
    let releases = get_releases(
        &agent,
        repository,
        &download_args.connection_settings.headers,
        download_args.connection_settings.force_refresh,
    );
    let asset = get_asset_or_exit(&releases, &download_args, &compiled_asset_pattern);

    // drop immutable reference and get a mutable reference
    let repository = &mut download_args.repository;

    // printing to stderr, since posix (or unix?)
    // says progress is written to stderr
    // this makes sense especially if we pipe the name
    // into a script: the script gets the downloaded
    // file name and the user can still see the progress
    eprintln!(r#"Downloading "{}""#, &asset.name);

    let url_buffer: String;
    let url = if matches!(repository.website, GitWebsite::GitHub) {
        download_args
            .connection_settings
            .headers
            .push("Accept: application/octet-stream".to_string());
        download_args
            .connection_settings
            .headers
            .push("X-GitHub-Api-Version: 2022-11-28".to_string());
        url_buffer = get_github_asset_api_url(&repository.owner, &repository.name, asset.id);
        url_buffer.as_str()
    } else {
        &asset.browser_download_url
    };

    let response = make_get_request(&agent, url, &download_args.connection_settings.headers)
        .unwrap_or_else(|e| {
            eprintln!("Error downloading file:\n{e}");
            process::exit(1);
        });

    let out_filename = &asset.name;

    let out_file = File::create(out_filename).unwrap_or_else(|e| {
        eprintln!("Error creating file:\n{e}");
        process::exit(1);
    });

    eprintln!("Writing to file \"{}\"", &out_filename);

    let content_length_option = get_content_length(&response);
    let pb_option = create_and_init_progress_bar(content_length_option);

    stream_response_into_file(response, out_file, &pb_option);

    if let Some(ref pb) = pb_option {
        pb.finish();
        eprintln!();
    }

    eprintln!(r#"Successfully wrote to file "{}""#, &out_filename);
    if download_args.print_filename {
        print!(r#"{}"#, &out_filename)
    }
}

fn main() {
    let args = arguments::Arguments::parse();

    match args.command_mode {
        arguments::CommandMode::Query(query_args) => match query_args.query_type {
            arguments::QueryType::Releases(releases_query_args) => {
                print_releases(releases_query_args);
                exit(0);
            }
            arguments::QueryType::Assets(assets_query_args) => {
                print_assets(assets_query_args);
                exit(0);
            }
        },
        arguments::CommandMode::Download(download_args) => {
            download_assets(download_args);
            exit(0);
        }
        arguments::CommandMode::DownloadAll(download_all_args) => {
            download_all_assets(download_all_args);
            exit(0);
        }
    };
}
