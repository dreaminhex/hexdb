# Releasing and distributing HexDB

Pushing a tag such as `v1.0.1` runs [.github/workflows/release.yml](../.github/workflows/release.yml), which builds every installer and publishes to every channel whose secret is set. This page lists what each channel needs once, then the routine for each release.

## The channels at a glance

| Channel | What users run | Hosted on | Set up once | Secret(s) |
| --- | --- | --- | --- | --- |
| Release files | download from GitHub | GitHub Releases | nothing | none |
| Windows installer | `hexdb-windows-x64.msi` | GitHub Releases | nothing (signing optional) | none |
| macOS installer | `hexdb-macos-universal.dmg` | GitHub Releases | nothing (signing optional) | `MACOS_*`, `APPLE_*` for signing |
| curl script | `curl -fsSL .../install.sh \| sh` | GitHub (raw file) | nothing | none |
| Docker | `docker run ghcr.io/dreaminhex/hexdb` | GitHub Container Registry | make the package public | none |
| APT | `sudo apt install hexdb` | GitHub Pages (`gh-pages` branch) | a GPG key, turn on Pages | `APT_GPG_PRIVATE_KEY`, `APT_GPG_PASSPHRASE` |
| Homebrew | `brew install dreaminhex/hexdb/hexdb` | the tap repo `dreaminhex/homebrew-hexdb` | create the tap repo, a token | `HOMEBREW_TAP_TOKEN` |
| winget | `winget install DreamInHex.HexDB` | `microsoft/winget-pkgs` | a token; first version is reviewed | `WINGET_TOKEN` |
| Chocolatey | `choco install hexdb` | community.chocolatey.org | an account and API key; versions are moderated | `CHOCOLATEY_API_KEY` |
| npm | `npm install @dreaminhex/hexdb` | npmjs.com | an account and token | `NPM_TOKEN` |
| PyPI | `pip install hexdb` | pypi.org | an account and token | `PYPI_TOKEN` |
| NuGet | `dotnet add package HexDB.Client` / `HexDB.EntityFrameworkCore` | nuget.org | an account and a trusted publishing policy | `NUGET_USER` (or `NUGET_API_KEY`) |

Every package name above was free on 2026-10-09: `hexdb` on PyPI, Chocolatey and crates.io, `HexDB.Client` and `HexDB.EntityFrameworkCore` on NuGet, `DreamInHex.HexDB` on winget. Claim them with the first release.

Secrets go in the HexDB repository under **Settings > Secrets and variables > Actions > New repository secret**. A job whose secret is missing is skipped, not failed, so you can turn channels on one at a time.

## The ODBC driver

The ODBC driver isn't a library that projects reference; it's a system driver that applications load by name (`Driver={HexDB}`). A .NET app uses it through `System.Data.Odbc`, Python through `pyodbc`, and Excel or Power BI through their ODBC data source. So it ships with HexDB itself rather than on NuGet or npm:

| Install | Where the driver ends up | Registered? |
| --- | --- | --- |
| Windows MSI (and winget, Chocolatey) | `C:\Program Files\HexDB\ODBC\hexdb_odbc.dll` | Yes, as "HexDB" (leave it out with `msiexec /i hexdb-windows-x64.msi ODBC=0`) |
| Debian/Ubuntu package (APT) | `/usr/lib/hexdb/libhexdb_odbc.so` | Yes, when unixODBC (`odbcinst`) is installed |
| macOS DMG | `/usr/local/hexdb/odbc/libhexdb_odbc.dylib` | No: add it to `odbcinst.ini` |
| Homebrew | `$(brew --prefix)/opt/hexdb/lib` | No: add it to `odbcinst.ini` |
| Archives and the curl script | `odbc/` in the archive, `~/.local/share/hexdb/odbc` | No (on Windows, `odbc\install-windows.ps1` registers it) |

## One-time setup

### Docker (GitHub Container Registry)

The workflow pushes `ghcr.io/dreaminhex/hexdb` with the tags `v1.0.1`, `1.0.1` and `latest`. A new container package starts private.

1. Open https://github.com/users/dreaminhex/packages/container/package/hexdb (it exists after the first release run).
2. **Package settings > Danger Zone > Change visibility > Public.**
3. Under **Manage Actions access**, check that the `hexdb` repository has **Write**.

### APT repository

The `apt` job signs a repository and pushes it to the `gh-pages` branch, served by GitHub Pages at https://dreaminhex.github.io/hexdb/apt.

1. Create a signing key, on any machine with GnuPG (Git Bash has `gpg`):

   ```bash
   gpg --quick-gen-key "HexDB packages <matthew@dreaminhex.com>" rsa4096 sign 5y
   gpg --list-secret-keys --keyid-format long          # note the key ID after rsa4096/
   gpg --armor --export-secret-keys <KEYID> > hexdb-apt-private.asc
   ```

2. Add the secrets: `APT_GPG_PRIVATE_KEY` is the whole content of `hexdb-apt-private.asc`, and `APT_GPG_PASSPHRASE` is the passphrase you chose (leave it unset if you chose none). Then delete the `.asc` file, and keep a backup of the key somewhere safe: losing it means every user has to import a new one.
3. Run a release (or rerun the `apt` job). It creates the `gh-pages` branch.
4. In the HexDB repo, **Settings > Pages > Build and deployment > Source: Deploy from a branch**, branch `gh-pages`, folder `/ (root)`, **Save**. Within a minute or two, https://dreaminhex.github.io/hexdb/apt/hexdb.asc should show the public key.

The job keeps the five newest versions (amd64 and arm64) in the repository.

### Homebrew tap

1. Create a **public** repository named exactly `homebrew-hexdb` under `dreaminhex`, with a README so it isn't empty.
2. Create a fine-grained personal access token: **GitHub Settings > Developer settings > Fine-grained tokens > Generate new token**. Repository access: only `dreaminhex/homebrew-hexdb`. Permissions: **Contents: Read and write**. Pick an expiry and put a reminder in your calendar.
3. Add it as `HOMEBREW_TAP_TOKEN`.

The `homebrew` job writes `Formula/hexdb.rb` into the tap with the version and checksums filled in. Users install with `brew install dreaminhex/hexdb/hexdb`. Getting into Homebrew's main repository (plain `brew install hexdb`) needs a project with some popularity; apply later at https://github.com/Homebrew/homebrew-core.

### winget

1. Create a classic personal access token with the **public_repo** scope (**Developer settings > Tokens (classic)**). wingetcreate uses it to fork `microsoft/winget-pkgs` into your account and open a pull request.
2. Add it as `WINGET_TOKEN`.

On the first release, the `winget` job submits the manifests in [packaging/winget](winget) with the MSI's checksum filled in; after that, it runs `wingetcreate update`. The first pull request needs two things from you on GitHub: agree to Microsoft's CLA when the bot asks (comment `@microsoft-github-policy-service agree`), and answer any reviewer questions. Validation and review usually take one to three days. `winget install DreamInHex.HexDB` works once it's merged.

### Chocolatey

1. Create an account at https://community.chocolatey.org/account/Register and confirm the email.
2. Copy your API key from **Account > API Keys** and add it as `CHOCOLATEY_API_KEY`.

The `chocolatey` job packs [packaging/chocolatey](chocolatey) (which runs the MSI silently) and pushes it. Every version goes through automated validation and verification, and the first one through a human moderator, which can take several days. You'll get emails; fix anything they flag and push a new version. `choco install hexdb` works once it's approved.

### npm

1. Create an account at https://www.npmjs.com/signup and turn on two-factor authentication.
2. **Access Tokens > Generate New Token > Granular Access Token.** Packages and scopes: **Read and write**, all packages (the package doesn't exist yet). Tick **Bypass two-factor authentication** so CI can publish. Add it as `NPM_TOKEN`.
3. After the first publish, narrow the token to the `hexdb` package, or switch to npm's trusted publishing (package **Settings > Trusted publishing**, GitHub Actions, workflow `release.yml`) and delete the token.

### PyPI

1. Create an account at https://pypi.org/account/register/ and turn on two-factor authentication.
2. **Account settings > API tokens > Add API token**, scope **Entire account** (the project doesn't exist until the first upload). Add it as `PYPI_TOKEN`.
3. After the first release, replace it with a token scoped to the `hexdb` project.

### NuGet

1. Sign in at https://www.nuget.org with a Microsoft account.
2. Use trusted publishing, so no long-lived key is stored: under your account menu, **Trusted Publishing > Create**. Repository owner `dreaminhex`, repository `hexdb`, workflow file `release.yml`, environment blank.
3. Add a secret `NUGET_USER` with your nuget.org user name (the profile name, not the email). The `nuget` job then exchanges GitHub's identity token for a one-hour key on each release.

   Alternatively, skip steps 2 and 3 and create an API key: **API Keys > Create**, scope **Push new packages and package versions**, glob `HexDB.*`, saved as `NUGET_API_KEY`.
3. Optional but worth it: ask NuGet to reserve the `HexDB.` prefix for your account (email account@nuget.org, see https://learn.microsoft.com/nuget/nuget-org/id-prefix-reservation). Reserved packages get a verified checkmark and nobody else can publish `HexDB.Something`.

New packages take a few minutes to validate and index before `dotnet add package` finds them.

### Signing the installers (optional, recommended)

Both installers work unsigned, with a warning the first time:

- **Windows:** SmartScreen says "Windows protected your PC"; the user clicks **More info > Run anyway**. winget and Chocolatey users don't see it.
- **macOS:** opening `HexDB.pkg` says Apple couldn't check it for malicious software; the user goes to **System Settings > Privacy & Security** and clicks **Open Anyway**. Homebrew and the curl script aren't affected.

To remove the macOS warning, join the Apple Developer Program ($99 a year), then:

1. In Xcode (**Settings > Accounts > Manage Certificates**) or at developer.apple.com, create a **Developer ID Application** and a **Developer ID Installer** certificate.
2. In Keychain Access, select both certificates with their private keys, **Export** them into one `.p12` file with a password.
3. Add the secrets: `MACOS_CERTS_P12` (the output of `base64 -i certs.p12`), `MACOS_CERTS_PASSWORD`, `MACOS_APP_IDENTITY` (for example `Developer ID Application: Your Name (ABCDE12345)`, exactly as Keychain shows it), `MACOS_INSTALLER_IDENTITY` (the `Developer ID Installer: ...` name), `APPLE_ID` (your Apple ID email), `APPLE_TEAM_ID` (the 10-character team ID), and `APPLE_APP_PASSWORD` (an app-specific password from https://account.apple.com).

The `dmg` job then signs the binaries, signs and notarizes the package, and staples the ticket. This path hasn't run yet, so watch the first signed build.

For Windows, the usual choice is Azure Trusted Signing (about $10 a month, after Microsoft verifies the publisher) or an OV certificate from a certificate authority. The workflow doesn't sign Windows files yet; that's a follow-up once you have one.

## Making a release

1. Set the version everywhere (crates, drivers, admin UI, OpenAPI, manifests) and commit:

   ```bash
   python scripts/set-version.py 1.0.1
   git commit -am "Release 1.0.1"
   ```

2. Make sure CI on `main` is green, then tag:

   ```bash
   git tag v1.0.1
   git push origin main v1.0.1
   ```

3. Watch **Actions > Release**. It takes about 25 minutes, most of it the Rust builds.
4. Edit the release on GitHub to add highlights above the generated notes.

A failed job can be rerun from the run's page without retagging. To redo a release from scratch, delete the GitHub release and the tag (`git push --delete origin v1.0.1 && git tag -d v1.0.1`), fix, commit and tag again. Don't redo a version that already reached npm, PyPI, NuGet or Chocolatey: those registries never accept the same version twice. Use the next patch version instead.

## Checking a release

```bash
# Release files and checksums
curl -fsSL https://github.com/dreaminhex/hexdb/releases/latest/download/SHA256SUMS

# Docker
docker run --rm --entrypoint hexdb ghcr.io/dreaminhex/hexdb:latest --version

# APT (on Ubuntu or Debian)
apt-cache policy hexdb

# Homebrew
brew update && brew info dreaminhex/hexdb/hexdb

# Drivers
npm view @dreaminhex/hexdb version
pip index versions hexdb
curl -s https://api.nuget.org/v3-flatcontainer/hexdb.client/index.json

# winget and Chocolatey, once approved
winget show DreamInHex.HexDB
choco search hexdb --exact
```

## What users run

These are the commands the website's Get started section shows.

```bash
# Windows: the installer, or
winget install DreamInHex.HexDB
choco install hexdb

# macOS: the DMG, or
brew install dreaminhex/hexdb/hexdb

# Linux and macOS, no root
curl -fsSL https://raw.githubusercontent.com/dreaminhex/hexdb/main/packaging/install.sh | sh

# Debian and Ubuntu
curl -fsSL https://dreaminhex.github.io/hexdb/apt/hexdb.gpg | sudo tee /usr/share/keyrings/hexdb.gpg > /dev/null
echo "deb [signed-by=/usr/share/keyrings/hexdb.gpg] https://dreaminhex.github.io/hexdb/apt stable main" | sudo tee /etc/apt/sources.list.d/hexdb.list
sudo apt update && sudo apt install hexdb
sudo systemctl enable --now hexdb

# Docker
docker run --rm --entrypoint hexdb ghcr.io/dreaminhex/hexdb secret     # prints a new key; keep it safe
docker run -d --name hexdb -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
  -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:..." ghcr.io/dreaminhex/hexdb
```

After any desktop install, `hexdb start` creates the user's configuration and encryption key on first run (in `%LOCALAPPDATA%\HexDB`, `~/Library/Application Support/HexDB` or `~/.config/hexdb`) and prints where the generated administrator password is. The Debian package and Homebrew's service use `/etc/hexdb` and `$(brew --prefix)/etc/hexdb` instead.
