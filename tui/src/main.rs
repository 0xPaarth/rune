use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use notify::{EventKind, RecursiveMode, Watcher};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, Mutex},
    time::timeout,
};

#[cfg(windows)]
const BIN_NAME: &str = "solution.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "solution";

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

mod theme {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(16, 16, 40);
    pub const PRIMARY: Color = Color::Cyan;
    pub const SECONDARY: Color = Color::Magenta;
    pub const TERTIARY: Color = Color::Yellow;
    pub const TEXT: Color = Color::White;
    pub const MUTED: Color = Color::Gray;
    pub const SUCCESS: Color = Color::Green;
    pub const FAIL: Color = Color::Red;
}

const RUNE_LOGO: &str = "██████  ██   ██  ██████  ██████
██   ██  ██ ██   ██      ██   ██
██████    ███    ██████  ██████
██   ██  ██ ██   ██      ██ ██
██████  ██   ██  ██████  ██  ██ ";

const VERSION_LINE: &str = "v0.1.0  ·  'Ingestion Bridge'";

const DEFAULT_TIME_LIMIT_MS: u64 = 2000;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DIFF_MAX_LINES: usize = 5;
const RATING_DELTA: i32 = 200;
const REC_COUNT: usize = 5;
const WEAK_TAG_COUNT: usize = 5;
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const API_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Structs & state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestCase {
    id: String,
    input: String,
    expected_output: String,
}

/// The daemon writes the *entire* ICodeforcesPayload object to
/// test_cases.json (not just the array), so accept both shapes.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TestCasesFile {
    Payload {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        rating: Option<i32>,
        #[serde(rename = "timeLimitMs", default)]
        time_limit_ms: Option<u64>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(rename = "testCases")]
        test_cases: Vec<TestCase>,
    },
    Bare(Vec<TestCase>),
}

#[derive(Debug, Clone)]
struct Problem {
    contest_id: String,
    index: String,
    name: Option<String>,
    rating: Option<i32>,
    tags: Vec<String>,
    dir: PathBuf,
    time_limit_ms: u64,
    test_cases: Vec<TestCase>,
}

#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    Pass { ms: u64 },
    Fail { got: String, ms: u64 },
    Tle,
    Rte { stderr: String, code: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AppState {
    Idle,
    Loaded,
    Compiling,
    Testing,
    Review,
    GitSyncing,
    FullDiff,
}

#[derive(Debug, Clone)]
enum RecsState {
    Idle,
    Loading,
    Loaded(Vec<Recommendation>),
    Error(String),
}

struct App {
    state: AppState,
    problem: Option<Problem>,
    /// One slot per test case for the current run; None = not finished yet.
    results: Vec<Option<Verdict>>,
    /// Set => the whole run is a CE; shown instead of per-test verdicts.
    compile_error: Option<String>,
    /// Last failed git step (set on push failure, cleared on next action).
    push_error: Option<String>,
    /// Tracks which Problems have already been recorded to the analytics DB
    /// during this session, so re-tests don't try to re-insert (the DB
    /// dedupes via PRIMARY KEY too, but this skips the round-trip).
    recorded: HashSet<(String, String)>,
    /// Dashboard data.
    weak_tags: Vec<String>,
    user_max_rating: Option<i32>,
    recs: RecsState,
    recs_selected: ListState,
    /// Cursor for the Review-state test case list (left pane); the right
    /// pane shows the diff for whichever index is selected here.
    results_selected: ListState,
    cf_handle: Option<String>,
    /// Monotonic run counter. Events from spawned tasks carry the run they
    /// belong to; anything from a superseded run is dropped, so a re-test or
    /// problem switch mid-run can't corrupt state.
    run: u64,
    spinner: usize,
    /// Set when a problem is ingested; cleared on reset back to Idle. The
    /// top bar renders elapsed time when this is `Some`.
    started_at: Option<Instant>,
    /// Vertical scroll offset for the FullDiff view (lines from the top of
    /// the rendered content). Reset to 0 every time we enter FullDiff.
    diff_offset: u16,
}

impl App {
    fn new(cf_handle: Option<String>) -> Self {
        Self {
            state: AppState::Idle,
            problem: None,
            results: Vec::new(),
            compile_error: None,
            push_error: None,
            recorded: HashSet::new(),
            weak_tags: Vec::new(),
            user_max_rating: None,
            recs: RecsState::Idle,
            recs_selected: ListState::default(),
            results_selected: ListState::default(),
            cf_handle,
            run: 0,
            spinner: 0,
            started_at: None,
            diff_offset: 0,
        }
    }
}

enum AppEvent {
    Key(KeyEvent),
    ProblemLoaded(Problem),
    StartTesting,
    CompileResult { run: u64, result: Result<(), String> },
    TestResult { run: u64, index: usize, verdict: Verdict },
    StartPush,
    PushResult { run: u64, result: Result<String, String> },
    StartFetchRecs { force_refresh: bool },
    RecommendationsReady(Result<Vec<Recommendation>, String>),
    Tick,
    TimerTick,
    Resize,
}

/// Restores the terminal on drop so any exit path (including `?` errors)
/// leaves the shell usable. `ratatui::init()` separately installs a panic
/// hook that does the same on panic.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

// ---------------------------------------------------------------------------
// Event handling: terminal input + ticker tasks
// ---------------------------------------------------------------------------

/// crossterm's `event::read()` is blocking, so it lives on a plain OS thread
/// and forwards into the async world via the unbounded channel.
fn spawn_terminal_events(tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || loop {
        match crossterm::event::read() {
            Ok(CtEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                if tx.send(AppEvent::Key(key)).is_err() {
                    break; // receiver gone -> app is shutting down
                }
            }
            Ok(CtEvent::Resize(_, _)) => {
                let _ = tx.send(AppEvent::Resize);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });
}

/// Drives the spinner animation while Compiling/Testing.
fn spawn_ticker(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(120));
        loop {
            interval.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

/// 1Hz tick used only to refresh the contest timer in the top bar. Kept
/// separate from the 120ms spinner ticker so spinner cadence isn't slowed.
fn spawn_timer_ticker(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if tx.send(AppEvent::TimerTick).is_err() {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Config: ~/.rune/config.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct Config {
    cf_handle: Option<String>,
}

fn rune_dir() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".rune")
}

/// Reads ~/.rune/config.toml. Missing file/key/parse-error all return
/// defaults silently — config is best-effort, never blocks startup. Writes
/// a commented skeleton on first run so the user can see what to fill in.
fn load_config() -> Config {
    let dir = rune_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("config.toml");

    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
        Err(_) => {
            let skeleton = "# Rune config\n# Uncomment and set your CF handle to get personalized recommendations\n# that exclude problems you've already solved.\n# cf_handle = \"your_handle_here\"\n";
            let _ = std::fs::write(&path, skeleton);
            Config::default()
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite analytics: ~/.rune/analytics.db
// ---------------------------------------------------------------------------

/// PRIMARY KEY (contest_id, problem_index) — a re-test of the same problem
/// is a no-op via INSERT OR IGNORE, so the spinner-mash test loop doesn't
/// inflate solve counts. `verdict` is reserved for future use (always PASS
/// today; FAIL/TLE rows would let us weight weak-tag computation later).
fn open_analytics_db() -> rusqlite::Result<Connection> {
    let dir = rune_dir();
    let _ = std::fs::create_dir_all(&dir);
    let conn = Connection::open(dir.join("analytics.db"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS solves (
            contest_id    INTEGER NOT NULL,
            problem_index TEXT    NOT NULL,
            name          TEXT    NOT NULL,
            rating        INTEGER,
            tags          TEXT    NOT NULL,
            timestamp     INTEGER NOT NULL,
            verdict       TEXT    NOT NULL,
            PRIMARY KEY (contest_id, problem_index)
        );",
    )?;
    Ok(conn)
}

fn record_solve(conn: &Connection, problem: &Problem) -> rusqlite::Result<()> {
    let contest_id: i64 = problem.contest_id.parse().unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT OR IGNORE INTO solves (contest_id, problem_index, name, rating, tags, timestamp, verdict)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            contest_id,
            &problem.index,
            problem.name.as_deref().unwrap_or(""),
            problem.rating,
            problem.tags.join(","),
            now,
            "PASS",
        ],
    )?;
    Ok(())
}

/// Returns (weak_tags, max_rating_solved). Weak tags = the tags appearing
/// least frequently across recent solves; if none in the window, fall back
/// to lifetime. Tags absent entirely from history are implicitly "weak"
/// but we surface what we can name first.
fn compute_weak_tags(conn: &Connection) -> rusqlite::Result<(Vec<String>, Option<i32>)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let window = now - 30 * 24 * 60 * 60;

    let collect_tags = |since: i64| -> rusqlite::Result<HashMap<String, u32>> {
        let mut stmt = conn.prepare("SELECT tags FROM solves WHERE timestamp >= ?")?;
        let rows = stmt.query_map([since], |r| r.get::<_, String>(0))?;
        let mut counts: HashMap<String, u32> = HashMap::new();
        for row in rows {
            for tag in row?.split(',').filter(|t| !t.is_empty()) {
                *counts.entry(tag.trim().to_string()).or_insert(0) += 1;
            }
        }
        Ok(counts)
    };

    let mut counts = collect_tags(window)?;
    if counts.is_empty() {
        counts = collect_tags(0)?;
    }

    let mut sorted: Vec<(String, u32)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let weak: Vec<String> = sorted
        .into_iter()
        .take(WEAK_TAG_COUNT)
        .map(|(t, _)| t)
        .collect();

    let max_rating: Option<i32> = conn
        .query_row(
            "SELECT MAX(rating) FROM solves WHERE rating IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    Ok((weak, max_rating))
}

// ---------------------------------------------------------------------------
// CF API client: problemset (cached) + user.status (optional)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CfProblem {
    #[serde(rename = "contestId")]
    contest_id: Option<u32>,
    index: String,
    name: String,
    #[serde(default)]
    rating: Option<i32>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CfProblemsetResult {
    problems: Vec<CfProblem>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "UPPERCASE")]
enum CfResponse<T> {
    Ok { result: T },
    Failed { comment: String },
}

#[derive(Debug, Clone, Deserialize)]
struct CfSubmission {
    problem: CfProblem,
    verdict: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProblemsetCache {
    fetched_at: u64,
    problems: Vec<CfProblem>,
}

fn problemset_cache_path() -> PathBuf {
    rune_dir().join("cf_problemset_cache.json")
}

fn load_problemset_cache() -> Option<ProblemsetCache> {
    let path = problemset_cache_path();
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_problemset_cache(cache: &ProblemsetCache) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(problemset_cache_path(), json);
    }
}

fn cache_is_fresh(cache: &ProblemsetCache) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(cache.fetched_at) < CACHE_TTL_SECS
}

async fn fetch_problemset(client: &reqwest::Client) -> Result<Vec<CfProblem>, String> {
    let resp: CfResponse<CfProblemsetResult> = client
        .get("https://codeforces.com/api/problemset.problems")
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse: {e}"))?;
    match resp {
        CfResponse::Ok { result } => Ok(result.problems),
        CfResponse::Failed { comment } => Err(format!("CF API: {comment}")),
    }
}

/// Returns (contestId, index) pairs of every problem the handle has at any
/// point submitted with verdict=OK. Failure (network, bad handle, etc.) is
/// non-fatal — caller treats `None` as "no filter".
async fn fetch_user_solved(
    client: &reqwest::Client,
    handle: &str,
) -> Result<HashSet<(u32, String)>, String> {
    let url = format!(
        "https://codeforces.com/api/user.status?handle={handle}&from=1&count=10000"
    );
    let resp: CfResponse<Vec<CfSubmission>> = client
        .get(&url)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?
        .json()
        .await
        .map_err(|e| format!("parse: {e}"))?;
    match resp {
        CfResponse::Ok { result } => Ok(result
            .into_iter()
            .filter(|s| s.verdict.as_deref() == Some("OK"))
            .filter_map(|s| Some((s.problem.contest_id?, s.problem.index)))
            .collect()),
        CfResponse::Failed { comment } => Err(format!("CF API: {comment}")),
    }
}

// ---------------------------------------------------------------------------
// Recommendation logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Recommendation {
    contest_id: u32,
    index: String,
    name: String,
    rating: Option<i32>,
    tags: Vec<String>,
    url: String,
    matched_tags: Vec<String>,
}

fn rank_recommendations(
    problemset: &[CfProblem],
    solved: Option<&HashSet<(u32, String)>>,
    weak_tags: &[String],
    user_max_rating: Option<i32>,
) -> Vec<Recommendation> {
    let weak_set: HashSet<&str> = weak_tags.iter().map(|s| s.as_str()).collect();

    // Rating band — if the user has no rating history, allow 800-1600 as a
    // gentle on-ramp instead of returning nothing.
    let (lo, hi) = match user_max_rating {
        Some(r) => (r - RATING_DELTA, r + RATING_DELTA),
        None => (800, 1600),
    };

    let mut scored: Vec<(i32, i32, Recommendation)> = problemset
        .iter()
        .filter_map(|p| {
            let cid = p.contest_id?;
            if let Some(solved) = solved {
                if solved.contains(&(cid, p.index.clone())) {
                    return None;
                }
            }
            let rating = p.rating?;
            if rating < lo || rating > hi {
                return None;
            }
            let matched: Vec<String> = p
                .tags
                .iter()
                .filter(|t| weak_set.contains(t.as_str()))
                .cloned()
                .collect();

            // Score: weak-tag matches (primary), then proximity to user's
            // current rating (secondary — gentle progression is better than
            // a big jump).
            let weak_score = matched.len() as i32;
            let target = user_max_rating.unwrap_or(1200);
            let rating_penalty = (rating - target).abs();

            let rec = Recommendation {
                contest_id: cid,
                index: p.index.clone(),
                name: p.name.clone(),
                rating: p.rating,
                tags: p.tags.clone(),
                url: format!("https://codeforces.com/contest/{cid}/problem/{}", p.index),
                matched_tags: matched,
            };
            // Sort by: weak_score DESC, rating_penalty ASC
            // Encode as a single (key1, key2) tuple where smaller is better
            // by negating weak_score.
            Some((-weak_score, rating_penalty, rec))
        })
        .collect();

    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(REC_COUNT).map(|(_, _, r)| r).collect()
}

// ---------------------------------------------------------------------------
// Startup scan: find the most recently modified test_cases.json
// ---------------------------------------------------------------------------

/// Walks `<root>/<contestId>/<index>/test_cases.json` (depth 2) and returns
/// the path with the newest mtime. Skips entries we can't stat — a missing
/// or partially-written file shouldn't crash startup. The directory layout
/// is bounded (~hundreds of problems even for an active user), so a
/// synchronous walk completes in well under 10ms on any reasonable disk.
fn find_latest_problem_json(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let contests = std::fs::read_dir(root).ok()?;
    for contest in contests.flatten() {
        if !contest.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(problems) = std::fs::read_dir(contest.path()) else {
            continue;
        };
        for problem in problems.flatten() {
            if !problem.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let json = problem.path().join("test_cases.json");
            let Ok(meta) = std::fs::metadata(&json) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else { continue };
            match &best {
                Some((b_mtime, _)) if mtime <= *b_mtime => {}
                _ => best = Some((mtime, json)),
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Runs the scan off the main async task so a slow disk can't delay the
/// first frame. The first `ProblemLoaded` event will transition the UI
/// to Loaded — same code path the watcher uses, no special-casing.
fn spawn_startup_scan(tx: mpsc::UnboundedSender<AppEvent>, root: PathBuf) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || find_latest_problem_json(&root))
            .await
            .ok()
            .flatten();
        if let Some(json) = result {
            if let Ok(problem) = load_problem(&json) {
                let _ = tx.send(AppEvent::ProblemLoaded(problem));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// FS watcher task
// ---------------------------------------------------------------------------

/// Watches `~/cp/Codeforces` recursively. The returned watcher must be kept
/// alive by the caller — dropping it stops the watch.
///
/// We only react to events whose path is a `test_cases.json`: the daemon
/// always writes that file when it creates/updates a problem directory, so
/// keying on it covers both "new directory" and "re-ingested problem"
/// without having to interpret bare directory events.
fn spawn_fs_watcher(
    tx: mpsc::UnboundedSender<AppEvent>,
    root: PathBuf,
) -> notify::Result<notify::RecommendedWatcher> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<PathBuf>();

    // notify invokes this callback on its own thread; UnboundedSender::send
    // is sync and thread-safe, so it bridges cleanly into tokio.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                for path in event.paths {
                    if path.file_name().is_some_and(|n| n == "test_cases.json") {
                        let _ = raw_tx.send(path);
                    }
                }
            }
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    tokio::spawn(async move {
        // A single write produces a burst of Create+Modify events; debounce
        // by ignoring repeats for the same file within the window.
        const DEBOUNCE: Duration = Duration::from_millis(500);
        let mut last_seen: HashMap<PathBuf, Instant> = HashMap::new();

        while let Some(json_path) = raw_rx.recv().await {
            let now = Instant::now();
            if last_seen
                .get(&json_path)
                .is_some_and(|t| now.duration_since(*t) < DEBOUNCE)
            {
                continue;
            }
            last_seen.insert(json_path.clone(), now);

            // Give the daemon a moment to finish the write before reading.
            tokio::time::sleep(Duration::from_millis(150)).await;

            if let Ok(problem) = load_problem(&json_path) {
                let _ = tx.send(AppEvent::ProblemLoaded(problem));
            }
        }
    });

    Ok(watcher)
}

/// Parses `.../{contestId}/{index}/test_cases.json` into a Problem.
fn load_problem(json_path: &Path) -> Result<Problem, String> {
    let index_dir = json_path
        .parent()
        .ok_or("test_cases.json has no parent directory")?;
    let index = index_dir
        .file_name()
        .ok_or("missing index directory name")?
        .to_string_lossy()
        .into_owned();
    let contest_id = index_dir
        .parent()
        .and_then(|p| p.file_name())
        .ok_or("missing contest directory name")?
        .to_string_lossy()
        .into_owned();

    let data = std::fs::read_to_string(json_path).map_err(|e| e.to_string())?;
    let parsed: TestCasesFile = serde_json::from_str(&data).map_err(|e| e.to_string())?;

    let (name, rating, time_limit_ms, tags, test_cases) = match parsed {
        TestCasesFile::Payload {
            name,
            rating,
            time_limit_ms,
            tags,
            test_cases,
        } => (name, rating, time_limit_ms, tags, test_cases),
        TestCasesFile::Bare(test_cases) => (None, None, None, Vec::new(), test_cases),
    };

    Ok(Problem {
        contest_id,
        index,
        name,
        rating,
        tags,
        dir: index_dir.to_path_buf(),
        time_limit_ms: time_limit_ms.filter(|&ms| ms > 0).unwrap_or(DEFAULT_TIME_LIMIT_MS),
        test_cases,
    })
}

// ---------------------------------------------------------------------------
// Execution engine: compile + run (async, never blocks the render loop)
// ---------------------------------------------------------------------------

/// Orchestrates a full run: compile, then each sample sequentially (to avoid
/// resource contention skewing timings). Reports back through the channel.
fn spawn_test_run(tx: mpsc::UnboundedSender<AppEvent>, problem: Problem, run: u64) {
    tokio::spawn(async move {
        let result = compile(&problem.dir).await;
        let compiled = result.is_ok();
        if tx.send(AppEvent::CompileResult { run, result }).is_err() {
            return;
        }
        if !compiled {
            return;
        }

        let limit = Duration::from_millis(problem.time_limit_ms);
        for (index, tc) in problem.test_cases.iter().enumerate() {
            let verdict = run_test(&problem.dir, tc, limit).await;
            if tx.send(AppEvent::TestResult { run, index, verdict }).is_err() {
                return;
            }
        }
    });
}

async fn compile(dir: &Path) -> Result<(), String> {
    let output = Command::new("g++")
        .args(["-std=c++17", "-O2", "-Wall", "-o", BIN_NAME, "solution.cpp"])
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| format!("failed to launch g++: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

async fn run_test(dir: &Path, tc: &TestCase, limit: Duration) -> Verdict {
    let mut child = match Command::new(dir.join(BIN_NAME))
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return Verdict::Rte {
                stderr: format!("failed to start solution: {e}"),
                code: None,
            }
        }
    };

    // Drain stdout/stderr concurrently with wait(): a program that emits more
    // than the pipe buffer would otherwise block forever and read as a TLE.
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    // Feed stdin from a task too; errors (EPIPE on early exit) are fine.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = tc.input.clone();
    tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });

    let started = Instant::now();
    let status = match timeout(limit, child.wait()).await {
        Err(_) => {
            let _ = child.kill().await;
            return Verdict::Tle;
        }
        Ok(Err(e)) => {
            return Verdict::Rte {
                stderr: format!("failed waiting on solution: {e}"),
                code: None,
            }
        }
        Ok(Ok(status)) => status,
    };
    let ms = started.elapsed().as_millis() as u64;

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        return Verdict::Rte {
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            code: status.code(),
        };
    }

    let got = String::from_utf8_lossy(&stdout).into_owned();
    if outputs_match(&tc.expected_output, &got) {
        Verdict::Pass { ms }
    } else {
        Verdict::Fail { got, ms }
    }
}

// ---------------------------------------------------------------------------
// Recommendation pipeline
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RecContext {
    db: Arc<Mutex<Connection>>,
    client: reqwest::Client,
    cf_handle: Option<String>,
}

fn spawn_fetch_recommendations(
    tx: mpsc::UnboundedSender<AppEvent>,
    ctx: RecContext,
    force_refresh: bool,
) {
    tokio::spawn(async move {
        let result = fetch_recommendations(&ctx, force_refresh).await;
        let _ = tx.send(AppEvent::RecommendationsReady(result));
    });
}

async fn fetch_recommendations(
    ctx: &RecContext,
    force_refresh: bool,
) -> Result<Vec<Recommendation>, String> {
    // 1. Local analytics
    let (weak_tags, user_max_rating) = {
        let db = ctx.db.lock().await;
        compute_weak_tags(&db).map_err(|e| format!("analytics: {e}"))?
    };

    // 2. Problemset (cache or fetch)
    let problems = if !force_refresh {
        match load_problemset_cache() {
            Some(c) if cache_is_fresh(&c) => c.problems,
            _ => {
                let problems = fetch_problemset(&ctx.client).await?;
                save_problemset_cache(&ProblemsetCache {
                    fetched_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    problems: problems.clone(),
                });
                problems
            }
        }
    } else {
        let problems = fetch_problemset(&ctx.client).await?;
        save_problemset_cache(&ProblemsetCache {
            fetched_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            problems: problems.clone(),
        });
        problems
    };

    // 3. Optional user.status (failure is non-fatal — fall through with None)
    let solved = if let Some(handle) = &ctx.cf_handle {
        fetch_user_solved(&ctx.client, handle).await.ok()
    } else {
        None
    };

    Ok(rank_recommendations(
        &problems,
        solved.as_ref(),
        &weak_tags,
        user_max_rating,
    ))
}

// ---------------------------------------------------------------------------
// Git sync pipeline
// ---------------------------------------------------------------------------

/// Stages, commits, and pushes the entire workspace. Operates from
/// `~/cp/Codeforces/` (the workspace root) because the daemon `git init`s
/// there — running from a problem subdir would fail until cwd is fixed.
/// The commit subject embeds the current problem for log readability.
fn spawn_push(tx: mpsc::UnboundedSender<AppEvent>, problem: Problem, run: u64) {
    tokio::spawn(async move {
        let result = run_push(&problem).await;
        let _ = tx.send(AppEvent::PushResult { run, result });
    });
}

async fn run_push(problem: &Problem) -> Result<String, String> {
    let root = problem
        .dir
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| "could not determine workspace root".to_string())?;

    let commit_msg = format!(
        "solve: CF {}{} ({}){}",
        problem.contest_id,
        problem.index,
        problem
            .rating
            .map(|r| r.to_string())
            .unwrap_or_else(|| "unrated".to_string()),
        problem
            .name
            .as_deref()
            .map(|n| format!(" — {n}"))
            .unwrap_or_default(),
    );

    // 1. git add .
    git_step(root, &["add", "."]).await?;

    // 2. git commit -m "..."   ("nothing to commit" => no-op success)
    let commit_out = run_git(root, &["commit", "-m", &commit_msg]).await?;
    if !commit_out.status.success() {
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&commit_out.stdout),
            String::from_utf8_lossy(&commit_out.stderr)
        );
        let benign = combined.contains("nothing to commit")
            || combined.contains("no changes added")
            || combined.contains("nothing added to commit");
        if !benign {
            return Err(format!("git commit failed:\n{}", combined.trim()));
        }
    }

    // 3. git push origin main
    git_step(root, &["push", "origin", "main"]).await?;

    Ok("Pushed to origin/main.".to_string())
}

async fn git_step(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let out = run_git(cwd, args).await?;
    if out.status.success() {
        Ok(())
    } else {
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Err(format!("git {} failed:\n{}", args[0], combined.trim()))
    }
}

/// `Command::output()` internally drains stdout AND stderr concurrently
/// with the wait, so we get the compile-side pipe-deadlock protection
/// for free — no manual reader tasks needed here.
async fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("failed to spawn `git {}`: {e}", args.join(" ")))
}

// ---------------------------------------------------------------------------
// Diffing logic
// ---------------------------------------------------------------------------

/// CP-style normalization: trailing whitespace on each line is irrelevant,
/// as are trailing empty lines.
fn normalize_output(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

fn outputs_match(expected: &str, got: &str) -> bool {
    normalize_output(expected) == normalize_output(got)
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

fn ui(frame: &mut Frame, app: &mut App) {
    // Paint the whole viewport in theme BG so no black bleed shows
    // between widgets / around the frame.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::BG).fg(theme::TEXT)),
        frame.area(),
    );

    let [top, main, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_top_bar(frame, top, app);
    match app.state {
        AppState::Idle => render_dashboard(frame, main, app),
        AppState::Loaded => {
            if let Some(problem) = app.problem.clone() {
                render_problem(frame, main, &problem);
            }
        }
        AppState::Compiling => render_compiling(frame, main, app),
        AppState::Testing | AppState::Review => {
            if let Some(problem) = app.problem.clone() {
                render_results(frame, main, app, &problem);
            }
        }
        AppState::GitSyncing => render_pushing(frame, main, app),
        AppState::FullDiff => {
            if let Some(problem) = app.problem.clone() {
                render_full_diff(frame, main, app, &problem);
            }
        }
    }
    render_bottom_bar(frame, bottom, app.state);
}

fn render_top_bar(frame: &mut Frame, area: Rect, app: &App) {
    // FullDiff gets its own bespoke top bar: "RUNE | Diff View: Sample N".
    if app.state == AppState::FullDiff {
        let sample_id = app
            .results_selected
            .selected()
            .and_then(|i| app.problem.as_ref().and_then(|p| p.test_cases.get(i)))
            .map(|tc| pretty_id(&tc.id))
            .unwrap_or_else(|| "—".to_string());
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "RUNE",
                Style::default().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format!("Diff View: {sample_id}"),
                Style::default().fg(theme::TERTIARY).add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme::BG)),
            area,
        );
        return;
    }

    let [left, center, right] = Layout::horizontal([
        Constraint::Length(10),
        Constraint::Min(0),
        Constraint::Length(28),
    ])
    .areas(area);

    let brand = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "RUNE",
            Style::default().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(brand).style(Style::default().bg(theme::BG)),
        left,
    );

    // Center: live contest timer (⏱ MM:SS) — only when a problem is loaded.
    if let Some(started) = app.started_at {
        let elapsed = started.elapsed().as_secs();
        let mins = elapsed / 60;
        let secs = elapsed % 60;
        let timer = Line::from(Span::styled(
            format!("⏱ {mins:02}:{secs:02}"),
            Style::default().fg(theme::TERTIARY).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(
            Paragraph::new(timer)
                .style(Style::default().bg(theme::BG))
                .alignment(Alignment::Center),
            center,
        );
    } else {
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme::BG)),
            center,
        );
    }

    let spin = SPINNER_FRAMES[app.spinner % SPINNER_FRAMES.len()];
    let status = match (&app.state, &app.problem) {
        (AppState::Idle, _) => Span::styled(
            "Dashboard ",
            Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
        ),
        (_, None) => Span::styled("No Problem ", Style::default().fg(theme::MUTED)),
        (state, Some(p)) => {
            let id = format!("CF {} {}", p.contest_id, p.index);
            match state {
                AppState::Loaded => Span::styled(
                    format!("{id} │ Ready "),
                    Style::default().fg(theme::SUCCESS).add_modifier(Modifier::BOLD),
                ),
                AppState::Compiling => Span::styled(
                    format!("{id} │ {spin} Compiling… "),
                    Style::default().fg(theme::TERTIARY),
                ),
                AppState::Testing => {
                    let done = app.results.iter().filter(|r| r.is_some()).count();
                    Span::styled(
                        format!("{id} │ {spin} Testing {done}/{} ", app.results.len()),
                        Style::default().fg(theme::PRIMARY),
                    )
                }
                AppState::Review => {
                    if app.compile_error.is_some() {
                        Span::styled(
                            format!("{id} │ Review — CE "),
                            Style::default().fg(theme::FAIL).add_modifier(Modifier::BOLD),
                        )
                    } else {
                        let passed = app
                            .results
                            .iter()
                            .filter(|r| matches!(r, Some(Verdict::Pass { .. })))
                            .count();
                        let total = app.results.len();
                        let color = if passed == total { theme::SUCCESS } else { theme::FAIL };
                        Span::styled(
                            format!("{id} │ Review — {passed}/{total} PASS "),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        )
                    }
                }
                AppState::GitSyncing => Span::styled(
                    format!("{id} │ {spin} Pushing to GitHub… "),
                    Style::default().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
                ),
                AppState::Idle | AppState::FullDiff => unreachable!(),
            }
        }
    };
    frame.render_widget(
        Paragraph::new(status)
            .style(Style::default().bg(theme::BG))
            .alignment(Alignment::Right),
        right,
    );
}

fn render_dashboard(frame: &mut Frame, area: Rect, app: &mut App) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);

    render_logo_panel(frame, left);

    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(right);
    render_analytics_panel(frame, top, app);
    render_recs_panel(frame, bottom, app);
}

fn render_logo_panel(frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = RUNE_LOGO
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        VERSION_LINE,
        Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "competitive programming, ergonomic",
        Style::default().fg(theme::MUTED),
    )));

    let centered = center_vertically(area, lines.len() as u16);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme::BG))
            .alignment(Alignment::Center),
        centered,
    );
}

fn render_analytics_panel(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Weak Tags",
        Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    if app.weak_tags.is_empty() {
        lines.push(Line::from(Span::styled(
            "No solve history yet.",
            Style::default().fg(theme::MUTED),
        )));
        lines.push(Line::from(Span::styled(
            "Solve a problem to populate.",
            Style::default().fg(theme::MUTED),
        )));
    } else {
        for tag in &app.weak_tags {
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(theme::MUTED)),
                Span::styled(tag.clone(), Style::default().fg(theme::TERTIARY)),
            ]));
        }
    }
    lines.push(Line::from(""));
    let rating_line = match app.user_max_rating {
        Some(r) => format!("Max solved rating: {r}"),
        None => "Max solved rating: —".to_string(),
    };
    lines.push(Line::from(Span::styled(
        rating_line,
        Style::default().fg(theme::TEXT),
    )));
    if let Some(handle) = &app.cf_handle {
        lines.push(Line::from(Span::styled(
            format!("CF handle: {handle}"),
            Style::default().fg(theme::MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "CF handle: (set in ~/.rune/config.toml)",
            Style::default().fg(theme::MUTED),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PRIMARY))
        .style(Style::default().bg(theme::BG))
        .title(Span::styled(
            " Analytics ",
            Style::default()
                .fg(theme::TERTIARY)
                .bg(theme::BG)
                .add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(theme::BG))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_recs_panel(frame: &mut Frame, area: Rect, app: &mut App) {
    let spin = SPINNER_FRAMES[app.spinner % SPINNER_FRAMES.len()];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PRIMARY))
        .style(Style::default().bg(theme::BG))
        .title(Span::styled(
            " Recommendations ",
            Style::default()
                .fg(theme::TERTIARY)
                .bg(theme::BG)
                .add_modifier(Modifier::BOLD),
        ));

    match &app.recs {
        RecsState::Idle => {
            let text = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Press [f] to fetch recommendations from Codeforces.",
                    Style::default().fg(theme::MUTED),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().bg(theme::BG))
                    .block(block),
                area,
            );
        }
        RecsState::Loading => {
            let text = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {spin} Fetching from Codeforces…"),
                    Style::default().fg(theme::PRIMARY),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().bg(theme::BG))
                    .block(block),
                area,
            );
        }
        RecsState::Error(msg) => {
            let text = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  CF API Error: {msg}"),
                    Style::default().fg(theme::FAIL),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press [f] to retry.",
                    Style::default().fg(theme::MUTED),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().bg(theme::BG))
                    .block(block)
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        RecsState::Loaded(recs) if recs.is_empty() => {
            let text = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No recommendations match your filters.",
                    Style::default().fg(theme::MUTED),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().bg(theme::BG))
                    .block(block),
                area,
            );
        }
        RecsState::Loaded(recs) => {
            let max_w = area.width.saturating_sub(6) as usize;
            let items: Vec<ListItem> = recs
                .iter()
                .map(|r| {
                    let rating = r
                        .rating
                        .map(|n| format!("[{n:>4}]"))
                        .unwrap_or_else(|| "[ ?  ]".to_string());
                    let separator = if r.tags.is_empty() { "" } else { " — " };
                    let tags_str = if r.tags.is_empty() {
                        String::new()
                    } else {
                        r.tags.join(", ")
                    };
                    // Single-line menu row: "[1600] Name — tag, tag"
                    let row = Line::from(vec![
                        Span::raw(" "),
                        Span::styled(
                            rating,
                            Style::default()
                                .fg(rating_color(r.rating))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            truncate(&r.name, max_w.saturating_sub(12)),
                            Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(separator, Style::default().fg(theme::MUTED)),
                        Span::styled(
                            truncate(&tags_str, max_w.saturating_sub(20)),
                            Style::default().fg(theme::MUTED),
                        ),
                    ]);
                    ListItem::new(row)
                })
                .collect();

            // Make sure selection is in range.
            if app.recs_selected.selected().is_none() {
                app.recs_selected.select(Some(0));
            }

            let list = List::new(items)
                .block(block)
                .style(Style::default().bg(theme::BG))
                .highlight_style(
                    Style::default()
                        .bg(theme::SECONDARY)
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶");
            frame.render_stateful_widget(list, area, &mut app.recs_selected);
        }
    }
}

fn rating_color(rating: Option<i32>) -> Color {
    match rating {
        Some(r) if r < 1200 => theme::MUTED,
        Some(r) if r < 1400 => theme::SUCCESS,
        Some(r) if r < 1600 => theme::PRIMARY,
        Some(r) if r < 1900 => Color::Blue,
        Some(r) if r < 2100 => theme::SECONDARY,
        Some(r) if r < 2400 => theme::TERTIARY,
        Some(_) => theme::FAIL,
        None => theme::MUTED,
    }
}

fn render_compiling(frame: &mut Frame, area: Rect, app: &App) {
    let spin = SPINNER_FRAMES[app.spinner % SPINNER_FRAMES.len()];
    let text = vec![
        Line::from(Span::styled(
            format!("{spin}  Compiling solution.cpp"),
            Style::default()
                .fg(theme::TERTIARY)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "g++ -std=c++17 -O2 -Wall",
            Style::default().fg(theme::MUTED),
        )),
    ];
    let centered = center_vertically(area, text.len() as u16);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().bg(theme::BG))
            .alignment(Alignment::Center),
        centered,
    );
}

fn render_pushing(frame: &mut Frame, area: Rect, app: &App) {
    let spin = SPINNER_FRAMES[app.spinner % SPINNER_FRAMES.len()];
    let text = vec![
        Line::from(Span::styled(
            format!("{spin}  Pushing workspace to GitHub"),
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "git add . → git commit → git push origin main",
            Style::default().fg(theme::MUTED),
        )),
    ];
    let centered = center_vertically(area, text.len() as u16);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().bg(theme::BG))
            .alignment(Alignment::Center),
        centered,
    );
}

fn render_problem(frame: &mut Frame, area: Rect, problem: &Problem) {
    const PREVIEW_LINES: usize = 3;

    let items: Vec<ListItem> = problem
        .test_cases
        .iter()
        .map(|tc| {
            let mut lines = vec![Line::from(Span::styled(
                pretty_id(&tc.id),
                Style::default().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
            ))];

            let input_lines: Vec<&str> = tc.input.lines().collect();
            let max_width = area.width.saturating_sub(6) as usize;
            for line in input_lines.iter().take(PREVIEW_LINES) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncate(line, max_width)),
                    Style::default().fg(theme::TEXT),
                )));
            }
            if input_lines.len() > PREVIEW_LINES {
                lines.push(Line::from(Span::styled(
                    "  …",
                    Style::default().fg(theme::MUTED),
                )));
            }
            lines.push(Line::from(""));
            ListItem::new(lines)
        })
        .collect();

    let title = match &problem.name {
        Some(name) => format!(" {} — Test Cases ({}) ", name, problem.test_cases.len()),
        None => format!(" Test Cases ({}) ", problem.test_cases.len()),
    };

    let list = List::new(items)
        .style(Style::default().bg(theme::BG))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::PRIMARY))
                .style(Style::default().bg(theme::BG))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(theme::TERTIARY)
                        .bg(theme::BG)
                        .add_modifier(Modifier::BOLD),
                )),
        );
    frame.render_widget(list, area);
}

fn render_results(frame: &mut Frame, area: Rect, app: &mut App, problem: &Problem) {
    // If a push just failed, carve off a banner above the verdict list.
    let (banner_area, results_area) = if let Some(err) = &app.push_error {
        let banner_h = (err.lines().count() as u16 + 2).min(area.height / 2).max(3);
        let [b, r] = Layout::vertical([Constraint::Length(banner_h), Constraint::Min(0)])
            .areas(area);
        (Some(b), r)
    } else {
        (None, area)
    };

    if let Some(b) = banner_area {
        let banner = Paragraph::new(app.push_error.as_deref().unwrap_or(""))
            .style(Style::default().bg(theme::BG).fg(theme::FAIL))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::FAIL))
                    .style(Style::default().bg(theme::BG))
                    .title(Span::styled(
                        " Push Failed ",
                        Style::default()
                            .fg(theme::FAIL)
                            .bg(theme::BG)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(banner, b);
    }

    let area = results_area;
    // CE replaces per-test verdicts entirely.
    if let Some(stderr) = &app.compile_error {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::FAIL))
            .style(Style::default().bg(theme::BG))
            .title(Span::styled(
                " Compilation Error (CE) ",
                Style::default()
                    .fg(theme::FAIL)
                    .bg(theme::BG)
                    .add_modifier(Modifier::BOLD),
            ));
        let para = Paragraph::new(stderr.as_str())
            .style(Style::default().bg(theme::BG).fg(theme::FAIL))
            .wrap(Wrap { trim: false })
            .block(block);
        frame.render_widget(para, area);
        return;
    }

    // 50/50 split: test case list on the left, diff for the selected one
    // on the right. Cursor moves the right pane.
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    render_test_case_list(frame, left, app, problem);
    render_diff_view(frame, right, app, problem);
}

fn render_test_case_list(frame: &mut Frame, area: Rect, app: &mut App, problem: &Problem) {
    let running_idx = app.results.iter().position(|r| r.is_none());
    let spin = SPINNER_FRAMES[app.spinner % SPINNER_FRAMES.len()];

    let items: Vec<ListItem> = problem
        .test_cases
        .iter()
        .zip(app.results.iter())
        .enumerate()
        .map(|(i, (tc, result))| {
            let id = pretty_id(&tc.id);
            let (label, color) = match result {
                None => {
                    if app.state == AppState::Testing && running_idx == Some(i) {
                        (format!("{spin} running…"), theme::PRIMARY)
                    } else {
                        ("queued".to_string(), theme::MUTED)
                    }
                }
                Some(Verdict::Pass { ms }) => (format!("PASS  ({ms} ms)"), theme::SUCCESS),
                Some(Verdict::Fail { ms, .. }) => (format!("FAIL  ({ms} ms)"), theme::FAIL),
                Some(Verdict::Tle) => {
                    (format!("TLE   (> {} ms)", problem.time_limit_ms), theme::TERTIARY)
                }
                Some(Verdict::Rte { code, .. }) => match code {
                    Some(c) => (format!("RTE   (exit {c})"), theme::SECONDARY),
                    None => ("RTE   (signal)".to_string(), theme::SECONDARY),
                },
            };
            let row = Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{id:<10}"),
                    Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]);
            ListItem::new(row)
        })
        .collect();

    // Make sure the selection is valid: clamp to the available range.
    if !app.results.is_empty() {
        let max = app.results.len().saturating_sub(1);
        let cur = app.results_selected.selected().unwrap_or(0).min(max);
        app.results_selected.select(Some(cur));
    } else {
        app.results_selected.select(None);
    }

    let title = match &problem.name {
        Some(name) => format!(" {name} — Test Cases "),
        None => " Test Cases ".to_string(),
    };
    let list = List::new(items)
        .style(Style::default().bg(theme::BG))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::PRIMARY))
                .style(Style::default().bg(theme::BG))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(theme::TERTIARY)
                        .bg(theme::BG)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(theme::SECONDARY)
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶");

    frame.render_stateful_widget(list, area, &mut app.results_selected);
}

fn render_diff_view(frame: &mut Frame, area: Rect, app: &App, problem: &Problem) {
    let idx = app.results_selected.selected().unwrap_or(0);
    let tc = problem.test_cases.get(idx);
    let result = app.results.get(idx).and_then(|r| r.as_ref());

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PRIMARY))
        .style(Style::default().bg(theme::BG))
        .title(Span::styled(
            " Diff ",
            Style::default()
                .fg(theme::TERTIARY)
                .bg(theme::BG)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    let max_width = inner.width.saturating_sub(4) as usize;

    if let Some(tc) = tc {
        lines.push(Line::from(Span::styled(
            "Input",
            Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
        )));
        push_diff_lines(&mut lines, &tc.input, max_width, theme::TEXT, " ");
        lines.push(Line::from(""));

        match result {
            None => {
                lines.push(Line::from(Span::styled(
                    "Awaiting run…",
                    Style::default().fg(theme::MUTED),
                )));
            }
            Some(Verdict::Pass { ms }) => {
                lines.push(Line::from(Span::styled(
                    format!("PASS in {ms} ms"),
                    Style::default()
                        .fg(theme::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Expected",
                    Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
                )));
                push_diff_lines(
                    &mut lines,
                    &tc.expected_output,
                    max_width,
                    theme::SUCCESS,
                    " ",
                );
            }
            Some(Verdict::Fail { got, ms }) => {
                lines.push(Line::from(Span::styled(
                    format!("FAIL in {ms} ms"),
                    Style::default().fg(theme::FAIL).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Expected",
                    Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
                )));
                push_diff_lines(
                    &mut lines,
                    &tc.expected_output,
                    max_width,
                    theme::FAIL,
                    "-",
                );
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Got",
                    Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
                )));
                push_diff_lines(&mut lines, got, max_width, theme::SUCCESS, "+");
            }
            Some(Verdict::Tle) => {
                lines.push(Line::from(Span::styled(
                    format!("TLE  (> {} ms)", problem.time_limit_ms),
                    Style::default()
                        .fg(theme::TERTIARY)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Expected",
                    Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
                )));
                push_diff_lines(
                    &mut lines,
                    &tc.expected_output,
                    max_width,
                    theme::MUTED,
                    " ",
                );
            }
            Some(Verdict::Rte { stderr, code }) => {
                let label = match code {
                    Some(c) => format!("RTE  (exit {c})"),
                    None => "RTE  (killed by signal)".to_string(),
                };
                lines.push(Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(theme::SECONDARY)
                        .add_modifier(Modifier::BOLD),
                )));
                if !stderr.trim().is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "stderr",
                        Style::default().fg(theme::SECONDARY).add_modifier(Modifier::BOLD),
                    )));
                    push_diff_lines(&mut lines, stderr, max_width, theme::FAIL, "!");
                }
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No test selected.",
            Style::default().fg(theme::MUTED),
        )));
    }

    let para = Paragraph::new(lines)
        .style(Style::default().bg(theme::BG))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

/// Full-screen scrollable diff: Expected | Got side-by-side when there's
/// room (≥80 cols), stacked otherwise. Auto-clamps `app.diff_offset` to the
/// last valid scroll position so the user can't run off the end.
fn render_full_diff(frame: &mut Frame, area: Rect, app: &mut App, problem: &Problem) {
    let idx = app.results_selected.selected().unwrap_or(0);
    let tc = problem.test_cases.get(idx);
    let result = app.results.get(idx).and_then(|r| r.as_ref());

    // Pull the "expected" and "got" strings (got may be empty for non-Fail
    // verdicts; we still want to show something useful — typically the
    // verdict label + expected output).
    let (expected_text, got_text, got_label, got_color) = match (tc, result) {
        (Some(tc), Some(Verdict::Fail { got, .. })) => (
            tc.expected_output.clone(),
            got.clone(),
            "Got".to_string(),
            theme::FAIL,
        ),
        (Some(tc), Some(Verdict::Pass { ms })) => (
            tc.expected_output.clone(),
            tc.expected_output.clone(),
            format!("Got (PASS in {ms} ms)"),
            theme::SUCCESS,
        ),
        (Some(tc), Some(Verdict::Tle)) => (
            tc.expected_output.clone(),
            String::new(),
            format!("Got (TLE > {} ms)", problem.time_limit_ms),
            theme::TERTIARY,
        ),
        (Some(tc), Some(Verdict::Rte { stderr, code })) => {
            let label = match code {
                Some(c) => format!("Got (RTE exit {c}) — stderr"),
                None => "Got (RTE signal) — stderr".to_string(),
            };
            (tc.expected_output.clone(), stderr.clone(), label, theme::SECONDARY)
        }
        (Some(tc), None) => (
            tc.expected_output.clone(),
            String::new(),
            "Got (awaiting run…)".to_string(),
            theme::MUTED,
        ),
        (None, _) => (
            String::new(),
            String::new(),
            "Got".to_string(),
            theme::MUTED,
        ),
    };

    let stacked = area.width < 80;
    let (left_area, right_area) = if stacked {
        let [a, b] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(area);
        (a, b)
    } else {
        let [a, b] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
        (a, b)
    };

    let expected_lines = normalize_output(&expected_text);
    let got_lines = normalize_output(&got_text);
    let total_lines = expected_lines.len().max(got_lines.len());

    // Viewport height inside the bordered block is area.height - 2 (top+bottom
    // border). One line at the bottom is reserved for the scroll indicator.
    let inner_h = right_area.height.saturating_sub(2);
    let viewport_h = inner_h.saturating_sub(1) as usize;
    let max_offset = total_lines.saturating_sub(viewport_h.max(1));
    if (app.diff_offset as usize) > max_offset {
        app.diff_offset = max_offset as u16;
    }
    let offset = app.diff_offset as usize;

    render_full_diff_pane(
        frame,
        left_area,
        " Expected ",
        &expected_lines,
        offset,
        viewport_h,
        theme::SUCCESS,
        total_lines,
    );
    render_full_diff_pane(
        frame,
        right_area,
        &format!(" {got_label} "),
        &got_lines,
        offset,
        viewport_h,
        got_color,
        total_lines,
    );
}

fn render_full_diff_pane(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    offset: usize,
    viewport_h: usize,
    body_color: Color,
    total_lines: usize,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PRIMARY))
        .style(Style::default().bg(theme::BG))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme::TERTIARY)
                .bg(theme::BG)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_width = inner.width.saturating_sub(2) as usize;
    let mut rendered: Vec<Line> = Vec::with_capacity(viewport_h + 1);
    if lines.is_empty() {
        rendered.push(Line::from(Span::styled(
            "  (empty)",
            Style::default().fg(theme::MUTED),
        )));
    } else {
        for line in lines.iter().skip(offset).take(viewport_h) {
            rendered.push(Line::from(Span::styled(
                format!(" {}", truncate(line, max_width.saturating_sub(1))),
                Style::default().fg(body_color),
            )));
        }
    }

    // Scroll indicator on the last inner row.
    let pct = if total_lines == 0 {
        100
    } else {
        let denom = total_lines.saturating_sub(viewport_h).max(1);
        ((offset.min(denom) * 100) / denom).min(100)
    };
    let indicator = Line::from(Span::styled(
        format!("-- {pct}% --"),
        Style::default().fg(theme::MUTED),
    ));

    // Split inner into body + 1-line indicator row.
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(rendered).style(Style::default().bg(theme::BG)),
        body,
    );
    frame.render_widget(
        Paragraph::new(indicator)
            .style(Style::default().bg(theme::BG))
            .alignment(Alignment::Center),
        footer,
    );
}

/// Pushes up to DIFF_MAX_LINES normalized lines, prefixed with `prefix`
/// (e.g. "-", "+", " ") and styled in `color`. Adds "…" if truncated.
fn push_diff_lines(
    lines: &mut Vec<Line>,
    content: &str,
    max_width: usize,
    color: Color,
    prefix: &str,
) {
    let normalized = normalize_output(content);
    if normalized.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {prefix} (empty)"),
            Style::default().fg(theme::MUTED),
        )));
        return;
    }
    for line in normalized.iter().take(DIFF_MAX_LINES) {
        lines.push(Line::from(Span::styled(
            format!("  {prefix} {}", truncate(line, max_width.saturating_sub(4))),
            Style::default().fg(color),
        )));
    }
    if normalized.len() > DIFF_MAX_LINES {
        lines.push(Line::from(Span::styled(
            "    …",
            Style::default().fg(theme::MUTED),
        )));
    }
}

fn render_bottom_bar(frame: &mut Frame, area: Rect, state: AppState) {
    let bindings: &[(&str, &str)] = match state {
        AppState::Idle => &[
            ("[↑↓]", "Select"),
            ("[f]", "Fetch"),
            ("[o]", "Open"),
            ("[q]", "Quit"),
        ],
        AppState::Loaded => &[("[t]", "Test"), ("[r]", "Reset"), ("[q]", "Quit")],
        AppState::Compiling | AppState::Testing | AppState::GitSyncing => &[("[q]", "Quit")],
        AppState::Review => &[
            ("[↑↓]", "Select"),
            ("[d]", "Full Diff"),
            ("[t]", "Re-test"),
            ("[p]", "Push"),
            ("[r]", "Reset"),
            ("[q]", "Quit"),
        ],
        AppState::FullDiff => &[
            ("[↑↓]", "Scroll"),
            ("[d/Esc]", "Back"),
            ("[q]", "Quit"),
        ],
    };

    let mut spans = vec![Span::raw(" ")];
    for (i, (key, label)) in bindings.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(theme::MUTED)));
        }
        spans.push(Span::styled(
            format!("{key} "),
            Style::default()
                .fg(theme::TERTIARY)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            *label,
            Style::default().fg(theme::TEXT),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG)),
        area,
    );
}

fn center_vertically(area: Rect, content_height: u16) -> Rect {
    let pad = area.height.saturating_sub(content_height) / 2;
    Rect {
        y: area.y + pad,
        height: content_height.min(area.height),
        ..area
    }
}

/// "sample-1" -> "Sample 1"
fn pretty_id(id: &str) -> String {
    let spaced = id.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// Char-boundary-safe truncation with a "..." marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

// ---------------------------------------------------------------------------
// Main: async event loop
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> io::Result<()> {
    let root = dirs::home_dir()
        .expect("could not determine home directory")
        .join("cp")
        .join("Codeforces");
    std::fs::create_dir_all(&root)?;

    let config = load_config();
    let db = Arc::new(Mutex::new(
        open_analytics_db().map_err(|e| io::Error::other(format!("analytics db: {e}")))?,
    ));
    let http_client = reqwest::Client::builder()
        .timeout(API_TIMEOUT)
        .user_agent("rune-tui/0.1")
        .build()
        .map_err(|e| io::Error::other(format!("reqwest: {e}")))?;
    let rec_ctx = RecContext {
        db: db.clone(),
        client: http_client,
        cf_handle: config.cf_handle.clone(),
    };

    let mut terminal = ratatui::init();
    let _guard = TerminalGuard;

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    spawn_terminal_events(tx.clone());
    spawn_ticker(tx.clone());
    spawn_timer_ticker(tx.clone());
    let _watcher = spawn_fs_watcher(tx.clone(), root.clone())
        .map_err(|e| io::Error::other(format!("fs watcher failed: {e}")))?;
    spawn_startup_scan(tx.clone(), root);

    let mut app = App::new(config.cf_handle.clone());

    // Prime the analytics panel from disk on startup so the dashboard
    // isn't empty before the first [f] press.
    {
        let conn = db.lock().await;
        if let Ok((tags, max_rating)) = compute_weak_tags(&conn) {
            app.weak_tags = tags;
            app.user_max_rating = max_rating;
        }
    }

    terminal.draw(|f| ui(f, &mut app))?;

    while let Some(event) = rx.recv().await {
        match event {
            AppEvent::Key(key) => match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Char('r') => {
                    app.run += 1; // invalidate any in-flight run
                    app.problem = None;
                    app.results.clear();
                    app.results_selected = ListState::default();
                    app.compile_error = None;
                    app.push_error = None;
                    app.state = AppState::Idle;
                    app.started_at = None;
                    app.diff_offset = 0;
                }
                KeyCode::Char('t')
                    if matches!(app.state, AppState::Loaded | AppState::Review) =>
                {
                    let _ = tx.send(AppEvent::StartTesting);
                }
                KeyCode::Char('p') if app.state == AppState::Review => {
                    let _ = tx.send(AppEvent::StartPush);
                }
                KeyCode::Char('d') if app.state == AppState::Review => {
                    // Only enter FullDiff if there's something to look at.
                    if !app.results.is_empty() {
                        app.diff_offset = 0;
                        app.state = AppState::FullDiff;
                    }
                }
                KeyCode::Char('d') | KeyCode::Esc if app.state == AppState::FullDiff => {
                    app.diff_offset = 0;
                    app.state = AppState::Review;
                }
                KeyCode::Down | KeyCode::Char('j') if app.state == AppState::FullDiff => {
                    app.diff_offset = app.diff_offset.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') if app.state == AppState::FullDiff => {
                    app.diff_offset = app.diff_offset.saturating_sub(1);
                }
                KeyCode::Char('f') if app.state == AppState::Idle => {
                    let _ = tx.send(AppEvent::StartFetchRecs { force_refresh: true });
                }
                KeyCode::Char('o') if app.state == AppState::Idle => {
                    if let RecsState::Loaded(recs) = &app.recs {
                        if let Some(idx) = app.recs_selected.selected() {
                            if let Some(r) = recs.get(idx) {
                                // open::that() spawns the OS handler; if it
                                // fails (no DISPLAY etc.) there's not much
                                // we can do — silent best-effort.
                                let _ = open::that(&r.url);
                            }
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j') if app.state == AppState::Idle => {
                    if let RecsState::Loaded(recs) = &app.recs {
                        if !recs.is_empty() {
                            let next = app
                                .recs_selected
                                .selected()
                                .map(|i| (i + 1).min(recs.len() - 1))
                                .unwrap_or(0);
                            app.recs_selected.select(Some(next));
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') if app.state == AppState::Idle => {
                    if let RecsState::Loaded(recs) = &app.recs {
                        if !recs.is_empty() {
                            let next = app.recs_selected.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
                            app.recs_selected.select(Some(next));
                        }
                    }
                }
                KeyCode::Down | KeyCode::Char('j')
                    if matches!(app.state, AppState::Review | AppState::Testing) =>
                {
                    if !app.results.is_empty() {
                        let next = app
                            .results_selected
                            .selected()
                            .map(|i| (i + 1).min(app.results.len() - 1))
                            .unwrap_or(0);
                        app.results_selected.select(Some(next));
                    }
                }
                KeyCode::Up | KeyCode::Char('k')
                    if matches!(app.state, AppState::Review | AppState::Testing) =>
                {
                    if !app.results.is_empty() {
                        let next = app
                            .results_selected
                            .selected()
                            .map(|i| i.saturating_sub(1))
                            .unwrap_or(0);
                        app.results_selected.select(Some(next));
                    }
                }
                _ => {}
            },
            AppEvent::ProblemLoaded(problem) => {
                app.run += 1; // a run for the previous problem is now stale
                app.problem = Some(problem);
                app.results.clear();
                app.results_selected = ListState::default();
                app.compile_error = None;
                app.push_error = None;
                app.state = AppState::Loaded;
                app.started_at = Some(Instant::now());
            }
            AppEvent::StartTesting => {
                if let Some(problem) = &app.problem {
                    if matches!(app.state, AppState::Loaded | AppState::Review) {
                        app.run += 1;
                        app.results = vec![None; problem.test_cases.len()];
                        app.results_selected.select(Some(0));
                        app.compile_error = None;
                        app.push_error = None;
                        app.state = AppState::Compiling;
                        spawn_test_run(tx.clone(), problem.clone(), app.run);
                    }
                }
            }
            AppEvent::CompileResult { run, result } if run == app.run => match result {
                Ok(()) => app.state = AppState::Testing,
                Err(stderr) => {
                    app.compile_error = Some(stderr);
                    app.state = AppState::Review;
                }
            },
            AppEvent::TestResult { run, index, verdict } if run == app.run => {
                if let Some(slot) = app.results.get_mut(index) {
                    *slot = Some(verdict);
                }
                if !app.results.is_empty() && app.results.iter().all(|r| r.is_some()) {
                    app.state = AppState::Review;
                    // Record solve to analytics if every test passed and we
                    // haven't already recorded this problem this session.
                    let all_pass = app
                        .results
                        .iter()
                        .all(|r| matches!(r, Some(Verdict::Pass { .. })));
                    if all_pass {
                        if let Some(problem) = &app.problem {
                            let key = (problem.contest_id.clone(), problem.index.clone());
                            if !app.recorded.contains(&key) {
                                let conn = db.lock().await;
                                let _ = record_solve(&conn, problem);
                                if let Ok((tags, max_rating)) = compute_weak_tags(&conn) {
                                    app.weak_tags = tags;
                                    app.user_max_rating = max_rating;
                                }
                                app.recorded.insert(key);
                            }
                        }
                    }
                }
            }
            AppEvent::StartFetchRecs { force_refresh } if app.state == AppState::Idle => {
                if !matches!(app.recs, RecsState::Loading) {
                    app.recs = RecsState::Loading;
                    spawn_fetch_recommendations(tx.clone(), rec_ctx.clone(), force_refresh);
                }
            }
            AppEvent::StartFetchRecs { .. } => continue,
            AppEvent::RecommendationsReady(result) => match result {
                Ok(recs) => {
                    app.recs_selected
                        .select(if recs.is_empty() { None } else { Some(0) });
                    app.recs = RecsState::Loaded(recs);
                }
                Err(msg) => {
                    app.recs = RecsState::Error(msg);
                }
            },
            AppEvent::StartPush => {
                if app.state == AppState::Review && app.compile_error.is_none() {
                    if let Some(problem) = &app.problem {
                        app.run += 1;
                        app.push_error = None;
                        app.state = AppState::GitSyncing;
                        spawn_push(tx.clone(), problem.clone(), app.run);
                    }
                }
            }
            AppEvent::PushResult { run, result } if run == app.run => match result {
                Ok(_msg) => {
                    // Clear context and return to Dashboard, ready for the next ingestion.
                    app.problem = None;
                    app.results.clear();
                    app.results_selected = ListState::default();
                    app.compile_error = None;
                    app.push_error = None;
                    app.state = AppState::Idle;
                    app.started_at = None;
                    app.diff_offset = 0;
                }
                Err(stderr) => {
                    app.push_error = Some(stderr);
                    app.state = AppState::Review;
                }
            },
            // Stale results from a superseded run: drop silently.
            AppEvent::CompileResult { .. }
            | AppEvent::TestResult { .. }
            | AppEvent::PushResult { .. } => continue,
            AppEvent::Tick => {
                let spinning_state = matches!(
                    app.state,
                    AppState::Compiling | AppState::Testing | AppState::GitSyncing
                );
                let spinning_recs = matches!(app.recs, RecsState::Loading);
                if spinning_state || spinning_recs {
                    app.spinner = app.spinner.wrapping_add(1);
                } else {
                    continue; // nothing animated; skip the redraw
                }
            }
            AppEvent::TimerTick => {
                // Only redraw when the timer is actually visible — Idle has
                // no started_at, and FullDiff hides the top bar entirely.
                if app.started_at.is_none() || app.state == AppState::FullDiff {
                    continue;
                }
            }
            AppEvent::Resize => {}
        }
        terminal.draw(|f| ui(f, &mut app))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_daemon_payload() {
        let json = r#"{
            "contestId": 1922,
            "index": "C",
            "name": "Closest to the Right",
            "url": "https://codeforces.com/contest/1922/problem/C",
            "rating": 1600,
            "tags": ["binary search"],
            "timeLimitMs": 2000,
            "memoryLimitMb": 256,
            "testCases": [
                { "id": "sample-1", "input": "5\n1 2 3 4 5\n", "expectedOutput": "3\n" }
            ]
        }"#;
        let parsed: TestCasesFile = serde_json::from_str(json).unwrap();
        match parsed {
            TestCasesFile::Payload {
                name,
                rating,
                time_limit_ms,
                tags,
                test_cases,
            } => {
                assert_eq!(name.as_deref(), Some("Closest to the Right"));
                assert_eq!(rating, Some(1600));
                assert_eq!(time_limit_ms, Some(2000));
                assert_eq!(tags, vec!["binary search".to_string()]);
                assert_eq!(test_cases.len(), 1);
                assert_eq!(test_cases[0].id, "sample-1");
                assert_eq!(test_cases[0].input, "5\n1 2 3 4 5\n");
                assert_eq!(test_cases[0].expected_output, "3\n");
            }
            TestCasesFile::Bare(_) => panic!("expected Payload variant"),
        }
    }

    #[test]
    fn parses_bare_array() {
        let json = r#"[{ "id": "sample-1", "input": "1\n", "expectedOutput": "2\n" }]"#;
        let parsed: TestCasesFile = serde_json::from_str(json).unwrap();
        match parsed {
            TestCasesFile::Bare(cases) => assert_eq!(cases.len(), 1),
            TestCasesFile::Payload { .. } => panic!("expected Bare variant"),
        }
    }

    #[test]
    fn outputs_match_ignores_trailing_whitespace() {
        assert!(outputs_match("1 2 3\n", "1 2 3"));
        assert!(outputs_match("1 2 3  \n\n\n", "1 2 3\n"));
        assert!(outputs_match("a\nb \nc\n", "a\nb\nc"));
        assert!(outputs_match("", "\n\n"));
    }

    #[test]
    fn outputs_match_detects_real_differences() {
        assert!(!outputs_match("1 2 3\n", "1 2 4\n"));
        assert!(!outputs_match("a\nb\n", "a\n"));
        // Leading/internal whitespace IS significant.
        assert!(!outputs_match("1 2\n", "1  2\n"));
        assert!(!outputs_match("a\n\nb\n", "a\nb\n"));
    }

    #[test]
    fn pretty_id_formats_sample_ids() {
        assert_eq!(pretty_id("sample-1"), "Sample 1");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
    }

    fn mk_problem(cid: u32, idx: &str, rating: i32, tags: &[&str]) -> CfProblem {
        CfProblem {
            contest_id: Some(cid),
            index: idx.to_string(),
            name: format!("{cid}{idx}"),
            rating: Some(rating),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn rank_filters_by_rating_band_and_solved() {
        let problems = vec![
            mk_problem(1, "A", 800, &["greedy"]),   // below band
            mk_problem(2, "A", 1500, &["dp"]),      // in band, weak tag dp
            mk_problem(3, "A", 1500, &["greedy"]),  // in band, no weak match
            mk_problem(4, "A", 1500, &["dp"]),      // already solved
            mk_problem(5, "A", 2200, &["dp"]),      // above band
        ];
        let mut solved = HashSet::new();
        solved.insert((4u32, "A".to_string()));
        let weak = vec!["dp".to_string()];
        let recs = rank_recommendations(&problems, Some(&solved), &weak, Some(1500));

        // 1500 dp ranks #1 (matched weak tag); 1500 greedy ranks #2 (same proximity, no match).
        // 800/2200 filtered out, contest 4 filtered as solved.
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].contest_id, 2);
        assert_eq!(recs[0].matched_tags, vec!["dp".to_string()]);
        assert_eq!(recs[1].contest_id, 3);
        assert!(recs[1].matched_tags.is_empty());
    }

    #[test]
    fn rank_prefers_closer_rating_when_weak_score_ties() {
        let problems = vec![
            mk_problem(1, "A", 1700, &["dp"]),
            mk_problem(2, "A", 1500, &["dp"]),
            mk_problem(3, "A", 1600, &["dp"]),
        ];
        let weak = vec!["dp".to_string()];
        let recs = rank_recommendations(&problems, None, &weak, Some(1500));
        // All three match the same weak tag; tie broken by |rating - 1500|.
        assert_eq!(recs[0].contest_id, 2); // |1500-1500|=0
        assert_eq!(recs[1].contest_id, 3); // |1600-1500|=100
        assert_eq!(recs[2].contest_id, 1); // |1700-1500|=200
    }

    #[test]
    fn rank_falls_back_to_default_band_with_no_history() {
        let problems = vec![
            mk_problem(1, "A", 1000, &["greedy"]),
            mk_problem(2, "A", 2000, &["greedy"]), // outside 800-1600 default
        ];
        let recs = rank_recommendations(&problems, None, &[], None);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].contest_id, 1);
    }
}
