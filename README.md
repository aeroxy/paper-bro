# paper-bro

Paste an arXiv paper and get an interactive course, published on
[AERO](https://aero.oops.wtf). No login. One paper gets one course: whoever
asks first starts the build, and everyone after that is sent straight to it.

Live at `https://aero.oops.wtf/paper-bro`. On any paper, swap `arxiv.org`
for `aero.oops.wtf/paper-bro` in the address.

Why each part is built the way it is lives in its module's doc comment
(`src/*.rs`, `shell/*.js`).

## What runs where

| Piece | Choice |
|---|---|
| Server | Rust: axum, tokio. One binary serving the SPA, the API, SSE progress, and the build workers |
| Database | Neon Postgres via sqlx (raw SQL). Migrations in `migrations/` run at boot |
| Front end | React 19 + Vite + Tailwind 3, built with bun, embedded with rust-embed |
| Model | Muse Spark Open on Baseten, or any OpenAI-compatible server, through `src/llm.rs` (no agent framework) |
| Browser QA | `chrome-headless-shell` (Chrome for Testing, pinned), driven over CDP with chromiumoxide |
| Publishing | AERO's `/mcp` endpoint with a personal access token; courses live at `/p/arxiv-<id>` |
| Deploy | One binary and a headless Chromium behind a reverse proxy at `/paper-bro` |

A build goes: ingest → outline → lessons (6 in parallel) → browser QA →
review → repair (at most two rounds) → publish. Each stage checkpoints to
`course_artifacts`, so a restarted job resumes instead of paying again.
Progress streams to the paper page over SSE.

### Safety model, in one paragraph

Courses are served from the host's own origin, next to whatever else it
runs, and they contain model-written code influenced by untrusted paper text.
So the browser confines that code:

- Lesson prose goes through ammonia.
- Every lab runs in a sandboxed iframe with an opaque origin (`sandbox="allow-scripts"`).
- Quiz and glossary text is rendered as text.
- A CSP `<meta>` limits scripts, images and network calls.

`paper-bro qa-sample` shows a lab failing to read storage, cookies or the
network from inside its iframe.

## Local development

```sh
cp .env.example .env          # fill in NEON_DB, BASETEN_API_KEY, AERO_TOKEN
mkdir -p .chrome && cd .chrome && \
  curl -sSLO https://storage.googleapis.com/chrome-for-testing-public/156.0.8078.47/mac-arm64/chrome-headless-shell-mac-arm64.zip && \
  unzip -q chrome-headless-shell-mac-arm64.zip && rm chrome-headless-shell-mac-arm64.zip && cd ..
(cd frontend && bun install && bun run build)
cargo run -- serve            # http://127.0.0.1:8083/paper-bro/
```

`PUBLISH=0`, the default, keeps finished courses in the database and serves
them at `/paper-bro/preview/<alias>` instead of publishing to AERO. A dry-run
server starts no build workers unless `WORKERS` is set, so the Build button
only queues; `cargo run -- build <id>` builds one course in the foreground.

Point local runs at their own database (a Neon branch works): `serve` and
`build` write real rows, and a running worker builds whatever is queued.

Other commands:

```sh
cargo test                        # unit tests
cargo run -- qa-sample            # browser QA on a course with deliberately broken labs
cargo run -- ingest 1706.03762    # the text the model reads, written to tmp/
cargo run -- build 1706.03762     # build one course in the foreground (costs ~$1)
```

## Deploying

paper-bro is one binary plus a headless Chromium, and runs on any Linux host:

1. **Build.** Build the front end first; rust-embed bakes it into the binary
   at compile time.

   ```sh
   (cd frontend && bun install && bun run build)
   cargo build --release
   ```

   From macOS, `cargo zigbuild --release --target x86_64-unknown-linux-musl`
   gives a static Linux binary.
2. **Install the browser.** Use the pinned `chrome-headless-shell` linux64
   build from Chrome for Testing, and point `CHROME_PATH` at it. On
   Debian/Ubuntu it needs:

   ```sh
   sudo apt-get install -y fontconfig fonts-noto-core fonts-noto-color-emoji \
     libnss3 libatk1.0-0 libatk-bridge2.0-0 libcups2 libdrm2 libxkbcommon0 \
     libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libasound2 libpango-1.0-0 libcairo2
   ```

   `ldd` on the binary lists anything still missing.
3. **Configure.** Set the environment; `.env.example` lists every variable.
   In production that means `PUBLISH=1` and your `PUBLIC_ORIGIN`. If
   paper-bro runs on the same host as AERO, point `AERO_MCP_URL` at AERO's
   local port to skip the round trip through the proxy. The model API is
   Baseten unless you set `LLM_BASE_URL`, `LLM_MODEL` and, if the server wants
   one, `LLM_API_KEY`: any OpenAI-compatible server that streams usage and
   supports strict `json_schema` output will do. Costs are counted at Baseten's
   Muse Spark Open prices whatever the server, so set `DAILY_BUDGET_USD` to
   suit.
4. **Run** `paper-bro serve` under whatever supervises processes on your
   host: systemd, a container, or nohup.
5. **Route** `/paper-bro*` on the public origin to `LISTEN`, without stripping
   the prefix. With Caddy:

   ```caddy
   your-site.example {
       handle /paper-bro* {
           reverse_proxy 127.0.0.1:8083
       }
       handle {
           reverse_proxy 127.0.0.1:8082   # whatever else the site serves
       }
   }
   ```

Chromium's sandbox needs unprivileged user namespaces. Ubuntu 22.04 allows
them. Ubuntu 24.04 restricts them through AppArmor, so there the binary needs
an AppArmor profile. Never run it with `--no-sandbox`.

### Operating

- **Ports:** `LISTEN` (8083) is the public listener, which the proxy routes
  to. `ADMIN_LISTEN` (8085) is the admin listener; it only takes local
  connections and has no auth.
- **The AERO token** expires after at most 90 days. When it does, publishing
  fails with a message naming `/connect`. Mint a new token there, update
  the environment, and restart.
- **Spend:** set a budget on your model provider's account too.
  `DAILY_BUDGET_USD` is paper-bro's own guard.

Admin, from the host itself:

```sh
curl -s 127.0.0.1:8085/jobs                       # every course and its state
curl -s 127.0.0.1:8085/events/1706.03762          # one course's history, incl. visitor reports
curl -s -X POST 127.0.0.1:8085/retry/1706.03762   # requeue a failed course
curl -s -X POST 127.0.0.1:8085/rebuild/1706.03762 # throw away checkpoints and rebuild
curl -s -X POST 127.0.0.1:8085/remove/1706.03762  # unpublish (takedown); never rebuilt
```
