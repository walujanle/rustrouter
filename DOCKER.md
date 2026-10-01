# Docker

Run rustrouter in a container. Published image:
[`ghcr.io/walujanle/rustrouter`](https://github.com/walujanle/rustrouter/pkgs/container/rustrouter),
multi-platform `linux/amd64` + `linux/arm64`.

The image is the same gateway the release binaries are: one static binary with the dashboard
embedded, no Node and no Python at runtime.

---

# For users

## Quick start

```bash
docker run -d \
  -p 20129:20129 \
  -v "$HOME/.9router:/app/data" \
  -e DATA_DIR=/app/data \
  --name rustrouter \
  ghcr.io/walujanle/rustrouter:latest
```

The dashboard is at http://localhost:20129, the OpenAI-compatible API at
`http://localhost:20129/v1`.

## Manage the container

```bash
docker logs -f rustrouter     # view logs
docker stop rustrouter        # stop
docker start rustrouter       # start again
docker rm -f rustrouter       # remove
```

## Data persistence

```bash
-v "$HOME/.9router:/app/data" \
-e DATA_DIR=/app/data
```

`DATA_DIR` is where every writable file lands. Inside the container the app defaults to
`~/.9router`; a container has no persistent home, so the bind mount plus `DATA_DIR=/app/data` is
what makes state survive a restart. Mounting `$HOME/.9router` rather than a private directory is
deliberate: that is where a native install keeps the same database, so the container reads the
data you already have and a cutover goes both ways.

```text
$DATA_DIR/
├── db/
│   ├── data.sqlite       # main SQLite database
│   └── backups/          # pre-migration safety backups
├── machine-id            # stable identity for the CLI token
├── jwt-secret            # session signing key
└── auth/cli-secret       # CLI token salt
```

Host path: `$HOME/.9router/db/data.sqlite`. Container path: `/app/data/db/data.sqlite`.

The database schema is byte-identical to 9router's, so one `data.sqlite` can move between the two
installs and a container can take over the data you already have. Move it, do not point both at it
at once: two processes on one file is only safe under conditions listed in
`docs/DB-PARITY.md`, and a 9router that fell back to its `sql.js` driver overwrites the whole file
from a boot-time snapshot.

`machine-id` and `jwt-secret` matter for a cutover as much as the database. `machine-id` is the
identity the CLI token is derived from, so if a container is created without the mounted
`DATA_DIR` it generates a new one and that token stops working. `jwt-secret` signs dashboard
sessions, so regenerating it invalidates them.

## Optional environment

```bash
docker run -d \
  -p 20129:20129 \
  -v "$HOME/.9router:/app/data" \
  -e DATA_DIR=/app/data \
  -e PORT=20129 \
  -e HOSTNAME=0.0.0.0 \
  --name rustrouter \
  ghcr.io/walujanle/rustrouter:latest
```

`HOSTNAME` has to stay `0.0.0.0`. The gateway reads `HOSTNAME` to decide what to bind, and Docker
sets that variable to the container id, so leaving it unset inside a container makes the bind fail.

## Update

```bash
docker pull ghcr.io/walujanle/rustrouter:latest
docker rm -f rustrouter
# re-run the quick start command
```

To pin a version instead of following `latest`, use the numbered tag:

```bash
docker pull ghcr.io/walujanle/rustrouter:0.1.0
```

## Running as a non-root user

The entrypoint starts as root only to `chown` the mounted data dir to the `rustrouter` user
(uid 10001) and then drops privileges. If you would rather run rootless, pass `--user` with a uid
that can already write the mount:

```bash
docker run -d \
  -p 20129:20129 \
  -v "$HOME/.9router:/app/data" \
  --user "$(id -u):$(id -g)" \
  -e HOME=/tmp \
  ghcr.io/walujanle/rustrouter:latest
```

The entrypoint skips the chown when it is not root, so the uid you pass has to own the mounted
directory already. `HOME=/tmp` is there because the image default, `/app/data-home`, is owned by
uid 10001; the CLI-tool writers are the one feature that writes under `$HOME` rather than
`DATA_DIR`.

---

# For developers

## Build locally

```bash
docker build -t rustrouter .

docker run --rm -p 20129:20129 \
  -v "$HOME/.9router:/app/data" \
  -e DATA_DIR=/app/data \
  rustrouter
```

The build is three stages: `web` runs `npm ci && npm run build`, `build` compiles the release binary
with that `web/dist` embedded, `runtime` copies only the binary. `cmake` and `clang` are needed in
the build stage because `aws-lc-sys` (the rustls backend) compiles C and assembly; SQLite is bundled
by `libsqlite3-sys`, so the runtime image carries no system SQLite.

## Compose

```bash
docker compose up -d
```

`docker-compose.yml` runs the published image with a named volume and the required environment.

## Publish

Push a git tag `vX.Y.Z` whose version matches `PROJECT_VERSION` → GitHub Actions builds
`linux/amd64` and `linux/arm64` on native runners, health-checks each platform image, verifies the
assembled manifest and `/api/health`, then publishes:

- `ghcr.io/walujanle/rustrouter:X.Y.Z`
- `ghcr.io/walujanle/rustrouter:latest` (stable tags only)

```bash
echo "0.3.0" > PROJECT_VERSION
node scripts/set-version.mjs
git commit -am "chore: release 0.3.0"
git tag v0.3.0
git push origin HEAD v0.3.0
```

The workflow fails unless the tag, `PROJECT_VERSION` and every manifest agree, so a mistyped or
unbumped tag cannot publish a mislabelled image. `PROJECT_VERSION` must be plain `x.y.z`, so a
prerelease tag is rejected rather than published under a numbered image.

To republish an existing tag, run **Build and Push Docker Image** manually with `release_tag`
(for example `v0.1.0`) and leave `promote_latest` off unless the republish should become `latest`.

GHCR publishing uses the workflow's `GITHUB_TOKEN` with `packages: write`, so no stored secret is
needed. Forks publish to their own namespace automatically. Docker Hub is not published; add a
login step and a `docker buildx imagetools create` retag if you want a second registry.

Numbered image tags are mutable, since a republish replaces the manifest. Pin a digest when a
deployment has to be immutable:

```bash
docker pull ghcr.io/walujanle/rustrouter@sha256:<verified-digest>
```

Workflow: `.github/workflows/docker-publish.yml`
