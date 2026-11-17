# Contributing

Use the repository root for Cargo commands. Stable Rust and Python 3 are needed for
the local checks. For routine changes, run only tests and examples relevant to the
changed behavior, plus the shared static checks:

```sh
python3 scripts/check.py --skip-tests
```

The static checks run formatting, feature checks (including isolated `claude`,
`claude,tokio`, and `tools` with each native adapter), Clippy and rustdoc. Tests use real
local shells and a scripted agent; no account credentials or live API calls are needed.
The `test-support` feature enables that helper executable and the integration tests
that use it. Applications should leave it disabled.

The workspace also contains the unpublished `agent-link-supervisor`, `agent-link-daemon` and `agent-link-http` crates; default
Cargo commands still select only `agent-link`. Workspace static/full gates and MSRV
CI include all four packages. The supervisor bundles SQLite, so its build needs a C compiler.
For supervisor/daemon changes run relevant targets:

```sh
cargo test --locked -p agent-link-supervisor --all-features -- --test-threads=1
cargo test --locked -p agent-link-daemon --all-features -- --test-threads=1
```

These tests use isolated stores and offline native stand-ins, including process crashes
and launcher exit. They do not establish power-loss durability or vendor compatibility.

For a delivery change, for example:

```sh
cargo test --locked --all-features --lib agent::delivery::tests -- --test-threads=1
cargo test --locked --all-features --test agent_delivery -- --test-threads=1
```

Use the coverage table in [AGENTS.md](AGENTS.md#test-coverage-guidelines) to select
related targets and narrow unit tests with module or test-name filters. Reserve the
full gate, `python3 scripts/check.py`, for release preparation or an explicit request
for a full run. CI continues to run the full gate, including all non-live tests.

Run PTY tests serially: simultaneous `openpty` calls can fail on macOS. Each Git
worktree should use its own default `target` directory. Do not run ignored vendor
tests unless explicitly intended: they can make API calls and change CLI state.
Focused permission checks and follow-up validation are documented in
[permission validation](docs/permission-validation.md).

Add a regression test for a behavioral fix, update public API documentation, and
record externally visible changes in `CHANGELOG.md`. Preserve coverage for the
minimal library (`--no-default-features`) and optional features. The gate checks `acp` and `codex`
independently and with `tokio`; its fake-peer tests run without credentials. See
[Codex adapter evidence](docs/rpc-and-native-adapters.md#codex-app-server-codex) before making compatibility claims.

See [release preparation](#release-preparation) for package validation.

The `observers` feature is checked independently without defaults by the shared static
gate; its bounded fanout unit tests and serial native PTY fence test require no credentials.

The `tools` feature adds optional `tempfile` (3.10+ for permission-aware directory
creation), `serde_json` and `url` (HTTP endpoint syntax validation) dependencies.
Keep the URL parser's transitive dependencies compatible with Rust 1.85 when updating
the lockfile. Its launch tests are offline:

```sh
cargo test --locked --all-features --lib tools::tests -- --test-threads=1
cargo test --locked --all-features --test tool_attachments -- --test-threads=1
```

Run the affected adapter tests when modifying MCP admission. CI's Rust 1.85 checks
include `--no-default-features --features tools`, workspace minimal/all-feature libraries,
and the daemon's all-feature targets. Temporary configuration is not live compatibility
evidence; see [application tools](docs/application-tools.md).

ACP offline subset validation: [adapter evidence](docs/rpc-and-native-adapters.md#agent-client-protocol-acp).

The optional `transcript` feature has an isolated no-default-features static gate and
focused `cargo test --locked --no-default-features --features transcript --lib transcript` coverage.

The unpublished `agent-link-http` workspace crate is covered by workspace Clippy,
rustdoc and Rust 1.85 checks. Run `cargo test --locked -p agent-link-http -- --test-threads=1`
for its offline local socket tests. It is a separate presentation package, never a core
network dependency or part of the published core archive. See [HTTP contracts](docs/workspace-crates.md#agent-link-http-http).

The shared static gate checks `approvals` alone and with each native adapter without
default features; the all-features MSRV gate includes approval callbacks. Callback
regressions use synthetic state and offline pipes; they do not establish live CLI support.

HTTP shutdown may wait for synchronous core recovery-boundary processing; socket
deadlines do not bound that lock wait. Daemon tests establish Unix local behavior,
not Windows runtime hosting or escaped-descendant containment.

## Documentation tiers

The tracked [public manual](docs/README.md) explains how to integrate the library,
including its defaults, limits and evidence. Keep it and its linked examples in Git
and in the core package. The static gate checks public links and, in a Git checkout,
checks that the manual and linked files are tracked.

`internal-docs/` is an optional, ignored local knowledge base for project development:
architecture notes, historical research, implementation audits and working notes.
It is excluded from crate archives and is not needed to build, test or release the
project. Keep public behavior contracts in rustdoc and the manual. Evidence referenced
by public metadata must live in tracked public docs or fixtures, such as the
[permission validation record](docs/permission-validation.md).

## Release preparation

Publication is a separate, explicit decision. Local validation does not publish.

1. Review the MIT license, contributor attribution, version and `CHANGELOG.md`.
   When source hosting is chosen, set the actual public `repository` URL in
   `Cargo.toml`; do not use a placeholder.
2. Run `python3 scripts/check.py` without `--skip-tests` on macOS and the Rust 1.85
   checks listed in CI. This release's verified native runtime scope is **macOS only**.
   Linux and Windows native CI jobs remain configured, with runtime results unverified;
   record passing native results before extending the verified-platform claim.
   MSRV checks use the committed lockfile; applications resolve the declared dependency
   ranges with their own lockfiles. Cross-compilation alone is not native runtime evidence.
3. Review the [native adapter boundaries](docs/rpc-and-native-adapters.md) and
   [permission validation record](docs/permission-validation.md) separately from
   fixture/pipe tests. Perform explicitly authorized live checks in an isolated
   environment and record the CLI versions and scope. Cursor remains explicitly
   **work in progress** in this release. Its authenticated `--force` startup/composer
   check is follow-up validation before removing that label, rather than a publication
   gate. A signed-out recording or skipped run does not establish authenticated
   composer compatibility or verified submission support.
4. Commit the intended release files, then run:

   ```sh
   cargo package --locked --list
   cargo package --locked
   cargo publish --locked --dry-run
   ```

   Inspect the archive: source, examples, fixtures, the public manual, compatibility
   evidence, contributor instructions and license must be present. Local
   `internal-docs/`, archives, worktrees, build outputs and host configuration must
   be absent. Validate a clean checkout and the extracted crate without private notes:

   ```sh
   cargo test --locked --all-features --test compatibility --test readme -- --test-threads=1
   cargo check --locked --all-features --examples
   ```

   The `agent-link-fake-agent` binary requires development-only `test-support` and
   remains disabled in ordinary builds. Default Cargo/package commands select the
   core `agent-link`; its archive excludes the supervisor, daemon and HTTP packages.
5. Publish to the chosen registry and create a version tag only after the explicit
   publication decision, then verify the registry artifact and generated docs.

The three sibling packages have `publish = false`. Publishing one requires a separate
decision, native platform validation and review of its ownership/runtime boundaries.
The shared static/full gates cover all four packages and require a C compiler for
the supervisor's bundled SQLite. Passing those gates does not establish native
provider interoperability, Windows daemon hosting, automatic runtime-fence recovery
or a bound on synchronous HTTP recovery-lock waits.
