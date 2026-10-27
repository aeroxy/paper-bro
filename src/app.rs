//! Shared state. Everything a page view needs is answered from memory, so a
//! visit doesn't wake Neon: the course map is loaded once at boot and written
//! through on every change, and paper previews are cached here too. paper-bro
//! runs as one process, which is what makes write-through enough.

use anyhow::Result;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, broadcast};

use crate::aero::Aero;
use crate::arxiv::{self, Meta};
use crate::config::Config;
use crate::db::{self, CourseRow, Event};
use crate::llm::Llm;

pub struct App {
    pub cfg: Config,
    pub pool: PgPool,
    pub llm: Llm,
    pub arxiv: arxiv::Client,
    pub aero: Aero,
    pub courses: RwLock<HashMap<String, CourseRow>>,
    channels: Mutex<HashMap<String, broadcast::Sender<Event>>>,
    pub wake: Notify,
    previews: Mutex<HashMap<String, (Instant, Option<Meta>)>>,
    builds_by_ip: Mutex<(chrono::NaiveDate, HashMap<u64, u32>)>,
    ip_hasher: RandomState,
    lab_errors_seen: Mutex<HashSet<String>>,
}

const PREVIEW_TTL: Duration = Duration::from_secs(6 * 3600);

impl App {
    pub async fn new(cfg: Config) -> Result<Self> {
        let pool = db::connect(&cfg.database_url).await?;
        let courses = db::all_courses(&pool).await?.into_iter().map(|c| (c.arxiv_id.clone(), c)).collect();
        Ok(Self {
            llm: Llm::new(&cfg.baseten_key, &cfg.model, &cfg.fallback_model, 8)?,
            arxiv: arxiv::Client::new()?,
            aero: Aero::new(&cfg.aero_mcp_url, &cfg.aero_token)?,
            cfg,
            pool,
            courses: RwLock::new(courses),
            channels: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            previews: Mutex::new(HashMap::new()),
            builds_by_ip: Mutex::new((chrono::Utc::now().date_naive(), HashMap::new())),
            ip_hasher: RandomState::new(),
            lab_errors_seen: Mutex::new(HashSet::new()),
        })
    }

    pub fn course(&self, id: &str) -> Option<CourseRow> {
        self.courses.read().unwrap().get(id).cloned()
    }

    /// Re-read one course into the map after a write.
    pub async fn refresh(&self, id: &str) -> Result<()> {
        match db::course(&self.pool, id).await? {
            Some(row) => self.courses.write().unwrap().insert(id.to_string(), row),
            None => self.courses.write().unwrap().remove(id),
        };
        Ok(())
    }

    pub fn subscribe(&self, id: &str) -> broadcast::Receiver<Event> {
        self.channels.lock().unwrap().entry(id.to_string()).or_insert_with(|| broadcast::channel(64).0).subscribe()
    }

    /// Record a progress event and push it to anyone watching.
    pub async fn emit(&self, id: &str, stage: &str, message: &str) {
        log::info!("[{id}] {stage}: {message}");
        match db::add_event(&self.pool, id, stage, message).await {
            Ok(ev) => {
                if let Some(tx) = self.channels.lock().unwrap().get(id) {
                    let _ = tx.send(ev);
                }
            }
            Err(e) => log::warn!("[{id}] couldn't record event: {e:#}"),
        }
        if let Some(c) = self.courses.write().unwrap().get_mut(id) {
            c.stage = Some(stage.to_string());
        }
    }

    /// arXiv metadata for a paper page, cached so repeat visits (and crawlers)
    /// don't spend arXiv's one-request-per-three-seconds budget.
    pub async fn preview(&self, id: &str) -> Result<Option<Meta>> {
        if let Some((at, meta)) = self.previews.lock().unwrap().get(id) {
            if at.elapsed() < PREVIEW_TTL {
                return Ok(meta.clone());
            }
        }
        // Only papers with a course are in Postgres; the rest come from arXiv.
        let meta = match self.course(id) {
            Some(_) => db::get_paper(&self.pool, id).await?,
            None => self.arxiv.meta(id).await?,
        };
        let mut cache = self.previews.lock().unwrap();
        if cache.len() > 5_000 {
            cache.clear();
        }
        cache.insert(id.to_string(), (Instant::now(), meta.clone()));
        Ok(meta)
    }

    /// Count a build against the caller's daily allowance. IPs are only ever
    /// held hashed, in memory, keyed per process.
    pub fn take_build(&self, ip: &str) -> bool {
        let key = self.ip_hasher.hash_one(ip);
        let mut guard = self.builds_by_ip.lock().unwrap();
        let today = chrono::Utc::now().date_naive();
        if guard.0 != today {
            *guard = (today, HashMap::new());
        }
        let n = guard.1.entry(key).or_insert(0);
        if *n >= self.cfg.builds_per_ip_per_day {
            return false;
        }
        *n += 1;
        true
    }

    /// Give back a build counted by `take_build` that didn't start a job
    /// (someone else had queued the same paper a moment earlier).
    pub fn refund_build(&self, ip: &str) {
        let key = self.ip_hasher.hash_one(ip);
        if let Some(n) = self.builds_by_ip.lock().unwrap().1.get_mut(&key) {
            *n = n.saturating_sub(1);
        }
    }

    /// True the first time this error is seen for this course since boot.
    pub fn first_lab_error(&self, key: String) -> bool {
        let mut seen = self.lab_errors_seen.lock().unwrap();
        if seen.len() > 20_000 {
            seen.clear();
        }
        seen.insert(key)
    }
}
