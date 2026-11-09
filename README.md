# paper-bro

Paste an arXiv paper and get an interactive course, published on
[GitHub Pages](https://github.com/paper-bro/paper-bro.github.io) at
`https://paper-bro.github.io/arxiv-<id>`. No login. One paper gets one course:
whoever asks first starts the build, and everyone after that is sent straight
to it.

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
| Model | Any OpenAI-compatible server (built against Muse Spark Open), through `src/llm.rs`, no agent framework |
| Browser QA | `chrome-headless-shell` (Chrome for Testing, pinned), driven over CDP with chromiumoxide |
| Publishing | The aero-oops-wtf GitHub App commits each course to `paper-bro/paper-bro.github.io` as `arxiv-<id>.html`; GitHub Pages serves it at `/arxiv-<id>` |
| Announcements | Each published course is posted once to the [paper-bro Telegram channel](https://t.me/arxiv_courses) by a bot, when `TELEGRAM_*` is set |
| Deploy | One binary and a headless Chromium behind a reverse proxy at `/paper-bro` |

A build goes: ingest → outline → lessons (6 in parallel) → browser QA →
review → repair (at most two rounds) → publish → a post on the Telegram
channel. Each stage checkpoints to
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
cp .env.example .env          # fill in NEON_DB, LLM_*, and GH_APP_KEY_PATH to publish
mkdir -p .chrome && cd .chrome && \
  curl -sSLO https://storage.googleapis.com/chrome-for-testing-public/156.0.8078.47/mac-arm64/chrome-headless-shell-mac-arm64.zip && \
  unzip -q chrome-headless-shell-mac-arm64.zip && rm chrome-headless-shell-mac-arm64.zip && cd ..
(cd frontend && bun install && bun run build)
cargo run -- serve            # http://127.0.0.1:8083/paper-bro/
```

`PUBLISH=0`, the default, keeps finished courses in the database and serves
them at `/paper-bro/preview/<alias>` instead of publishing them. A dry-run
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
cargo run -- announce             # print the Telegram posts not sent yet (PUBLISH=1 sends them)
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
   In production that means `PUBLISH=1`, your `PUBLIC_ORIGIN`, and the GitHub
   App's private key where `GH_APP_KEY_PATH` points (readable by paper-bro
   only). The App needs `contents: write` on `GH_REPO`. The model API is
   whatever OpenAI-compatible server `LLM_BASE_URL` (up to `/v1`), `LLM_MODEL`
   and, if it wants one, `LLM_API_KEY` point at. It must stream usage and
   support strict `json_schema` output. Costs are counted at Muse Spark Open's
   list price whatever the server, so set `DAILY_BUDGET_USD` to suit. The key
   travels as a bearer token, so use `https` unless the server is on this
   machine or a network you trust; paper-bro warns at startup if it would go
   over plain http to another host.
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
- **Publishing:** each course is one commit to `GH_REPO`, and GitHub Pages
  deploys it within a minute or so; a build waits for the page to answer
  before it calls the course ready. The App can't change Pages settings, so
  Pages is switched on once by hand: Settings → Pages → Deploy from a branch
  → `main`, `/ (root)`. At startup a publishing server logs whether it can
  get a token for the repo; "publishing will fail" means every build would.
- **Spend:** set a budget on your model provider's account too.
  `DAILY_BUDGET_USD` is paper-bro's own guard.
- **Updating a published course:** `paper-bro republish <arxiv id>` rebuilds a
  finished course's page from its stored lessons with the current page code,
  runs browser QA on it, and updates the page in place. It makes no model
  calls and needs no `LLM_*` settings. It refuses if QA finds a problem, if a
  figure differs from the one published, or if the page is over the size
  limit. Without `PUBLISH=1` it only writes the page to `tmp/`.
- **Telegram:** with `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` set, a
  publishing server posts each new course to the channel once; a rebuild or
  a republish doesn't post it again. A failed post is noted with the course's
  events and doesn't fail the build. `paper-bro announce [<arxiv id>]` posts
  a course published before, or every one not posted yet; without
  `PUBLISH=1` it only prints the messages. If a post's outcome isn't known
  for sure (Telegram's reply was lost, its id couldn't be recorded, or the
  server stopped mid-post), the course is left out of every retry until
  someone looks at the channel; `paper-bro announce <arxiv id> --again` posts
  it if it isn't there.

Admin, from the host itself:

```sh
curl -s 127.0.0.1:8085/jobs                       # every course and its state
curl -s 127.0.0.1:8085/events/1706.03762          # one course's history, incl. visitor reports
curl -s -X POST 127.0.0.1:8085/retry/1706.03762   # requeue a failed course
curl -s -X POST 127.0.0.1:8085/rebuild/1706.03762 # throw away checkpoints and rebuild
curl -s -X POST 127.0.0.1:8085/remove/1706.03762  # unpublish (takedown); never rebuilt
```
