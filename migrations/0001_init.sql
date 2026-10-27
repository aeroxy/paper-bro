-- One paper, one course. `courses.arxiv_id` is both the primary key and the
-- foreign key, so a second request for the same paper can only ever find the
-- first one's row (INSERT … ON CONFLICT DO NOTHING in src/db.rs).

create table papers (
  arxiv_id     text primary key,        -- canonical, no version: '1706.03762', 'hep-th/9711200'
  version      int  not null,
  title        text not null,
  authors      jsonb not null,
  abstract     text not null,
  categories   text[] not null,
  license_url  text,
  published_at timestamptz,
  source_kind  text,                    -- 'html' | 'latex', set by ingest
  fetched_at   timestamptz not null default now()
);

create table courses (
  arxiv_id      text primary key references papers,
  status        text not null check (status in ('queued','running','ready','failed','removed')),
  stage         text,
  attempts      int  not null default 0,
  lease_until   timestamptz,
  aero_page_id  text,
  aero_url      text,
  built_version int,
  model         text,
  usage         jsonb not null default '{}',
  error         text,
  requested_at  timestamptz not null default now(),
  started_at    timestamptz,
  finished_at   timestamptz
);
create index courses_queue on courses (status, requested_at);

-- Per-stage checkpoints, so a restarted job resumes instead of paying for
-- finished stages again. The heavy ones are pruned once a course is ready.
create table course_artifacts (
  arxiv_id   text not null references courses on delete cascade,
  key        text not null,             -- 'paper' | 'outline' | 'lesson:03' | 'review:1' | 'html'
  body       text not null,
  created_at timestamptz not null default now(),
  primary key (arxiv_id, key)
);

-- Replayed by the SSE progress feed; also holds the first report of each
-- distinct lab error from visitors.
create table course_events (
  id       bigserial primary key,
  arxiv_id text not null references courses on delete cascade,
  at       timestamptz not null default now(),
  stage    text not null,
  message  text not null
);
create index course_events_by_course on course_events (arxiv_id, id);
