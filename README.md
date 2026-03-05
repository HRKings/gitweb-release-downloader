# gitweb-release-downloader

A CLI tool to download release assets from **GitHub**, **Gitea** (including Forgejo), and **GitLab**.
You can also query a repository's releases and their assets.

The binary is called `grd`.

## Installation

### From source

```bash
git clone https://github.com/HRKings/gitweb-release-downloader.git
cd gitweb-release-downloader
cargo install --path .
```

## Commands

| Command          | Description                            |
| ---------------- | -------------------------------------- |
| `download`       | Download a single asset from a release |
| `download-all`   | Download assets from multiple releases |
| `query releases` | List release tags                      |
| `query assets`   | List assets in a release               |

## Usage

### `download` — single asset

Download the latest `.deb` from VSCodium:

```bash
grd download "github.com/VSCodium/vscodium" "\.deb$"
```

If the website type can be guessed from the URL (github.com, gitlab.com), you
don't need to specify it. Otherwise, pass `--website-type`:

```bash
grd download --website-type gitea codeberg.org/forgejo/forgejo ".*"
```

By default the latest non-prerelease is used. To pick a specific tag or allow
prereleases:

```bash
grd download "github.com/VSCodium/vscodium" "\.deb$" --tag 1.85.1.24019
grd download "github.com/VSCodium/vscodium" "\.deb$" --prerelease
```

Pipe the downloaded filename to another program with `--print-filename`:

```bash
filename=$(grd download "github.com/VSCodium/vscodium" "\.deb$" --print-filename)
sudo apt install "./$filename" && rm "$filename"
```

### `download-all` — bulk download

Download all assets across all matching releases into tag-name subdirectories:

```bash
grd download-all "github.com/user/repo"
```

This creates `<output_dir>/<tag>/<asset>` for every asset. Files that already
exist are skipped by default.

Filter releases and assets with regex patterns:

```bash
grd download-all "github.com/user/repo" \
  --release-pattern "^v1\." \
  --asset-pattern "linux.*amd64"
```

| Flag                | Short | Default | Description                                   |
| ------------------- | ----- | ------- | --------------------------------------------- |
| `--release-pattern` | `-e`  | `.*`    | Regex to filter release tags                  |
| `--asset-pattern`   | `-a`  | `.*`    | Regex to filter asset names                   |
| `--prerelease`      | `-p`  | `false` | Include prereleases                           |
| `--output-dir`      | `-o`  | `.`     | Output directory (tag subdirs created inside) |
| `--print-filenames` | `-f`  | `false` | Print downloaded file paths to stdout         |
| `--overwrite`       | `-x`  | `false` | Re-download files that already exist          |

Run again without `--overwrite` and existing files are skipped:

```
Skipping "./v1.0.0/asset.tar.gz" (already exists)
```

### `query` — inspect releases and assets

List the latest release tag:

```bash
grd query releases "github.com/VSCodium/vscodium"
```

List the last 5 releases, including prereleases:

```bash
grd query releases "github.com/VSCodium/vscodium" --count 5 --prerelease
```

List all assets of the latest release:

```bash
grd query assets "github.com/VSCodium/vscodium"
```

List assets matching a pattern from a specific tag:

```bash
grd query assets "github.com/VSCodium/vscodium" --tag 1.85.1.24019 --asset-pattern "\.deb$"
```

## Common options

These flags are available on all commands that make network requests:

| Flag              | Short | Description                                      |
| ----------------- | ----- | ------------------------------------------------ |
| `--website-type`  | `-w`  | Force website type (`github`, `gitea`, `gitlab`) |
| `--ip-type`       | `-i`  | Restrict to `ipv4` or `ipv6` (default: `any`)    |
| `--header`        |       | Custom HTTP header (`"Name: value"`), repeatable |
| `--force-refresh` | `-r`  | Bypass the release cache                         |

### Private repositories

For private repositories, pass an authorization header:

```bash
grd download "github.com/owner/private-repo" "asset" \
  --header "Authorization: Bearer ghp_YOUR_TOKEN"
```

## Caching

Release listings are cached for 1 hour under `$XDG_CACHE_HOME/grd/` (or
`~/.cache/grd/`). Use `--force-refresh` to bypass the cache.

## License

[GPL-3.0-only](LICENSE)
