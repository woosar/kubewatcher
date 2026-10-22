# Publishing kubewatcher to crates.io

How to get from this repository to `cargo install kubewatcher`.

## One-time setup

### 1. Create a crates.io account

1. Go to <https://crates.io> and click **Log in with GitHub**. There are no separate passwords.
2. Open **Account Settings** → **Profile**, add an email address and **verify it** (click the link in
   the mail). crates.io refuses to publish until the email is verified.

### 2. Create an API token

1. **Account Settings** → **API Tokens** → **New Token**.
2. Give it a name you'll recognise later, e.g. `laptop-publish`.
3. Pick an expiration (90 days is sensible; you can always make a new one).
4. Scopes: tick **publish-new** (needed for the very first publish) and **publish-update**.
   Optionally restrict it to the crate name `kubewatcher`.
5. Click **Generate Token** and **copy it now**. crates.io only shows it once.

### 3. Give the token to cargo

```sh
cargo login
```

Paste the token when it asks. Cargo stores it in a plain text file:

| OS | Token file |
|---|---|
| Linux / macOS | `~/.cargo/credentials.toml` |
| Windows | `%USERPROFILE%\.cargo\credentials.toml` |

The file looks like this:

```toml
[registry]
token = "cio_xxxxxxxxxxxxxxxxxxxxxxxx"
```

Treat it like a password:

- **It is not in this repository and must never be.** It lives in your home directory, not the project.
- On Linux, check that only you can read it: `chmod 600 ~/.cargo/credentials.toml`.
- Lost or leaked? Revoke it on crates.io under **API Tokens** and run `cargo login` with a new one.
- `cargo logout` removes it from the file again.

#### Optional: keep the token in the OS keyring instead of a file

Add this to `~/.cargo/config.toml` (not the project's), then run `cargo login` again:

```toml
[registry]
# Linux (GNOME keyring / KWallet via libsecret)
global-credential-providers = ["cargo:libsecret"]
# Windows: ["cargo:wincred"]   macOS: ["cargo:macos-keychain"]
```

#### Optional: CI (GitHub Actions)

Don't use `cargo login` there. Store the token as a repository secret
(GitHub → Settings → Secrets and variables → Actions → `CARGO_REGISTRY_TOKEN`), and cargo reads
it from the environment variable of the same name:

```yaml
- run: cargo publish
  env:
    CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
```

## Every release

1. Bump `version` in `Cargo.toml` (e.g. `0.1.0` → `0.1.1` for fixes, `0.2.0` for new features).
2. Check that it builds and the tests pass:
   ```sh
   cargo clippy --all-targets
   cargo test
   ```
3. See exactly what would be uploaded, and let cargo build it from that package:
   ```sh
   cargo package --list
   cargo publish --dry-run
   ```
4. Commit, tag and push. crates.io links to the GitHub repository.
   ```sh
   git commit -am "Release 0.1.1"
   git tag v0.1.1
   git push && git push --tags
   ```
5. Publish:
   ```sh
   cargo publish
   ```
6. Check it: <https://crates.io/crates/kubewatcher>, then `cargo install kubewatcher` on another machine.

## Things that can't be undone

- **A published version is permanent.** You can't delete it or upload the same version number again.
  If it's broken, publish a fixed version and **yank** the bad one:
  `cargo yank --version 0.1.1`. Yanking stops new projects from picking it, but it stays downloadable.
- **Everything in the package is public forever**, so check `cargo package --list` for anything
  that shouldn't be there.
- **The first publish claims the name** `kubewatcher` for your account. You can add co-owners later
  with `cargo owner --add <github-user>`.
