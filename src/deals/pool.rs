//! The station pool (§5.8, paper Algorithm 1). Every station keeps a FIFO
//! queue per activity (the paper's task type) and a number of execution
//! slots. Whenever a slot is free, the station dequeues the head of its
//! longest queue and routes
//! it: forward to the best-scoring neighbor (router.rs), or run it here. A
//! run ends in an answer or a split; a split's continuation (finished part
//! plus what remains) re-enters the same station's queue, where routing may
//! hand it to another station that resumes it. Finished tasks are judged
//! (qa.rs) and the verdict is learned by every station that executed part of
//! them (expertise.rs); successful runs join the station's memory.
//!
//!
//! Extension, not in the paper: a run may also end in a fork — independent
//! subtasks that enter the queue as ordinary tasks (routed, judged and
//! learned from on their own) while the parent waits; when the last one
//! finishes, the parent's continuation is queued with all their reports.
//!
//! The executor is injected, so the pool runs real subagents in the harness
//! and scripted ones in tests.

use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::oneshot;

use super::catalog::Station;
use super::expertise::{CostModel, Expertise, Outcome, Rng};
use super::memory::{Embedder, Memory, Trajectory};
use super::qa::{self, Ending, Evidence};
use super::router::{self, Ad};
use super::{Activity, Labels, short_model};
use crate::api::Message;
use crate::config::DealsConfig;
use crate::decide::Decider;

/// One execution segment handed to the executor.
pub struct Segment {
    /// The lead-facing id: agent-N.
    pub task: u32,
    pub model: String,
    pub labels: Labels,
    /// First line of the brief, for the UI.
    pub title: String,
    /// The full text the subagent gets: demonstrations, the brief, and for a
    /// continuation what earlier segments finished.
    pub brief: String,
    /// A follow-up's earlier transcript, run as its next task.
    pub resume: Option<(Vec<Message>, u32)>,
    /// A follow-up or continuation rather than a fresh start.
    pub continued: bool,
    /// Files and directories the task may write, as the lead or a fork
    /// declared them (`.`: anywhere); empty: read-only.
    pub writes: Vec<String>,
    /// The subagent may `fork` (forking on, nesting limit not reached).
    pub can_fork: bool,
    /// Wall-clock limit for this segment, by difficulty (`time_limit`).
    pub time_limit: Option<std::time::Duration>,
    /// The user's own request (the pipeline, §5.7): the report is the reply.
    pub from_user: bool,
}

/// One parallel subtask of a fork.
#[derive(Clone, Debug)]
pub struct ChildSpec {
    pub brief: String,
    pub writes: Vec<String>,
    /// From the decision model when the fork was made.
    pub labels: Labels,
}

pub enum SegmentEnd {
    Done(String),
    /// The subagent handed back: what it finished and what remains.
    Split { done: String, remaining: String },
    /// The subagent forked: parallel subtasks, then `then` with their reports.
    Fork { done: String, children: Vec<ChildSpec>, then: String },
    /// Ran out of turns or time.
    TurnLimit(String),
    Failed(String),
    Interrupted(String),
}

pub struct SegmentResult {
    pub end: SegmentEnd,
    pub cost: f64,
    pub secs: u64,
    pub transcript: Vec<Message>,
    /// Files the segment changed, from the ledger (shell-made ones too).
    pub files: Vec<String>,
}

pub type Exec = Arc<dyn Fn(Segment) -> Pin<Box<dyn Future<Output = SegmentResult> + Send>> + Send + Sync>;

/// What the lead gets when a task leaves the pool.
pub struct Finished {
    pub report: String,
    /// The station that ran the last segment (follow-ups go back to it).
    pub model: String,
    pub qa: f64,
    pub secs: u64,
    pub splits: u32,
    pub ok: bool,
}

/// A task in the pool right now, for the lead's view of the pipeline.
pub struct Live {
    pub id: u32,
    /// A fork's subtask: the task that forked it.
    pub parent: Option<u32>,
    pub labels: Labels,
    pub title: String,
    pub writes: Vec<String>,
    /// The station it is queued at or running on.
    pub model: String,
    pub state: LiveState,
    pub secs: u64,
    pub splits: u32,
}

pub enum LiveState {
    /// Tasks ahead of it at its station.
    Queued(usize),
    Running,
    /// Forked: the subtasks it waits for.
    Waiting(Vec<u32>),
}

/// Where a submitted task stands right after submission.
pub struct Receipt {
    pub model: String,
    /// Tasks ahead of it at that station; None once it is running.
    pub ahead: Option<usize>,
}

struct Task {
    id: u32,
    labels: Labels,
    brief: String,
    /// Capabilities the running model must have (`catalog::NEEDS`).
    needs: Vec<String>,
    hops: u32,
    splits: u32,
    prev: Option<usize>,
    at: usize,
    pinned: bool,
    resume: Option<(Vec<Message>, u32)>,
    /// The paper's S(x): stations that executed part of the task.
    executors: Vec<usize>,
    /// Finished parts from earlier segments, and what the last split left.
    parts: Vec<String>,
    remaining: Option<String>,
    cost: BTreeMap<String, f64>,
    commands: Vec<String>,
    files: Vec<String>,
    /// Steps and seconds of each segment, by station, for memory.
    segments: Vec<(usize, String, u64)>,
    embedding: Option<Vec<f32>>,
    started: Instant,
    /// The lead's receiver; None for a fork's subtask.
    done: Option<oneshot::Sender<Finished>>,
    /// A fork's subtask: whose results it joins, and how deep.
    parent: Option<u32>,
    depth: u32,
    writes: Vec<String>,
    /// A forked parent: the subtasks it is waiting for.
    waiting_for: std::collections::BTreeSet<u32>,
    /// Exploration draws, one per station, for this task's routing.
    draws: HashMap<usize, f64>,
    /// A station that just ran out of turns or time on this task: its
    /// continuation must run elsewhere.
    avoid: Option<usize>,
    /// The task has run out once: the rest of its routing goes by the
    /// estimates, never exploring again.
    no_explore: bool,
    /// The user's own request, straight from the pipeline.
    from_user: bool,
    /// Sent where it is by coverage: it runs there, no routing.
    covered: bool,
}

/// Queues are per activity; unlabelled tasks share one.
type Key = Option<Activity>;

#[derive(Default)]
struct Queues {
    waiting: BTreeMap<Key, VecDeque<u32>>,
    in_flight: BTreeMap<Key, u32>,
}

impl Queues {
    fn backlog(&self, t: Key) -> usize {
        self.waiting.get(&t).map_or(0, VecDeque::len) + *self.in_flight.get(&t).unwrap_or(&0) as usize
    }
    fn total(&self) -> usize {
        self.queued() + self.busy() as usize
    }
    fn busy(&self) -> u32 {
        self.in_flight.values().sum()
    }
    fn queued(&self) -> usize {
        self.waiting.values().map(VecDeque::len).sum()
    }
    /// Longest queue; ties by activity order (the paper's tie rule).
    fn longest(&self) -> Option<Key> {
        let mut best: Option<(Key, usize)> = None;
        for (t, q) in &self.waiting {
            if !q.is_empty() && best.is_none_or(|(_, n)| q.len() > n) {
                best = Some((*t, q.len()));
            }
        }
        best.map(|(t, _)| t)
    }
}

struct State {
    queues: Vec<Queues>,
    tasks: HashMap<u32, Task>,
    running: u32,
    /// The last station of every finished task, for follow-ups.
    history: HashMap<u32, usize>,
}

pub struct Pool {
    config: DealsConfig,
    stations: Vec<Station>,
    state: Mutex<State>,
    expertise: Mutex<Expertise>,
    memory: Option<Memory>,
    embedder: Option<Embedder>,
    decider: Option<Arc<Decider>>,
    exec: Exec,
    project: String,
    /// Agent ids, shared with the spawner: subtasks get agent-N ids too.
    ids: Arc<AtomicU32>,
    /// Draws for exploring routing (`[deals] explore`).
    rng: Mutex<Rng>,
}

/// Roughly one request every 7.5 s per busy subagent: a model allowed R
/// requests a minute keeps about R/8 agents moving.
const RPM_PER_SLOT: u32 = 8;

enum Step {
    Forwarded,
    Run(usize, u32),
}

impl Pool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: DealsConfig,
        stations: Vec<Station>,
        expertise: Expertise,
        memory: Option<Memory>,
        embedder: Option<Embedder>,
        decider: Option<Arc<Decider>>,
        exec: Exec,
        project: String,
        ids: Arc<AtomicU32>,
    ) -> Pool {
        let queues = stations.iter().map(|_| Queues::default()).collect();
        Pool {
            config,
            stations,
            state: Mutex::new(State { queues, tasks: HashMap::new(), running: 0, history: HashMap::new() }),
            expertise: Mutex::new(expertise),
            memory,
            embedder,
            decider,
            exec,
            project,
            ids,
            rng: Mutex::new(Rng::from_time()),
        }
    }

    fn index(&self, model: &str) -> Option<usize> {
        self.stations.iter().position(|s| s.model == model)
    }

    /// A segment's wall-clock limit: `segment_secs` for a moderate task,
    /// doubling per difficulty level above and halving per level below
    /// (unlabelled counts as moderate), within 1 and 30 minutes. None when
    /// `segment_secs` is 0.
    fn time_limit(&self, difficulty: Option<f64>) -> Option<std::time::Duration> {
        if self.config.segment_secs == 0 {
            return None;
        }
        let d = difficulty.unwrap_or(super::expertise::MODERATE);
        let secs = self.config.segment_secs as f64 * 2f64.powf(d - super::expertise::MODERATE);
        Some(std::time::Duration::from_secs_f64(secs.clamp(60.0, 1800.0)))
    }

    /// Slots open at a station right now: the configured count, bounded by
    /// the model's rate budget, halved while the provider is throttling it.
    fn capacity(&self, i: usize) -> u32 {
        let model = &self.stations[i].model;
        let by_rate = (crate::ratelimit::model_rpm(model) / RPM_PER_SLOT).max(1);
        let c = self.config.slots.min(by_rate).max(1);
        if crate::ratelimit::recently_throttled(model) { (c / 2).max(1) } else { c }
    }

    /// What a station advertises for a task with these labels: its backlog
    /// of that activity, its chance of success, its expected cost. The
    /// chance is the task's draw for this station when exploring (one per
    /// station per task, kept across hops so the task doesn't bounce), else
    /// the estimate. The cost is the cost model's expectation at the task's
    /// difficulty (`CostModel`), so an untried station is compared at a
    /// realistic price.
    #[allow(clippy::too_many_arguments)]
    fn ad(&self, st: &State, i: usize, labels: &Labels, expertise: &Expertise, rng: &mut Rng, draws: &mut HashMap<usize, f64>, costs: &CostModel, explore: bool) -> Ad {
        let s = &self.stations[i];
        let q = if explore {
            *draws.entry(i).or_insert_with(|| expertise.sample(&s.model, labels, rng))
        } else {
            expertise.p(&s.model, labels)
        };
        Ad { backlog: st.queues[i].backlog(labels.activity) as f64, q, cost: costs.expect(&s.model, s.cost_guess(), labels.difficulty) }
    }

    /// Too few outcomes yet and already holding a task: an untried station
    /// takes tasks one at a time.
    fn on_probation(&self, st: &State, j: usize, expertise: &Expertise) -> bool {
        expertise.outcomes(&self.stations[j].model) < self.config.probation as usize && st.queues[j].total() >= 1
    }

    /// Queue a new task. Lands at `ingress` when that model is an eligible
    /// station, otherwise at the least-loaded eligible station for its
    /// activity; routing takes it from there, among eligible stations only.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        self: &Arc<Self>,
        id: u32,
        labels: Labels,
        brief: String,
        needs: Vec<String>,
        writes: Vec<String>,
        ingress: Option<&str>,
        from_user: bool,
    ) -> Result<(Receipt, oneshot::Receiver<Finished>)> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.state.lock().unwrap();
            let queued: usize = st.queues.iter().map(Queues::queued).sum();
            if queued >= self.config.max_queued as usize {
                bail!(
                    "{queued} tasks are already waiting for a free slot — wait for some reports before filing more \
                     (back pressure, [deals] max_queued = {})",
                    self.config.max_queued
                );
            }
            if self.stations.is_empty() {
                bail!("the DEALS pool has no stations — run `tursi --stations probe`");
            }
            if let Some(bad) = needs.iter().find(|n| !super::catalog::NEEDS.contains(&n.as_str())) {
                bail!("unknown need {bad:?} — one of {}", super::catalog::NEEDS.join(", "));
            }
            let eligible: Vec<usize> = (0..self.stations.len()).filter(|&i| self.stations[i].can(&needs)).collect();
            if eligible.is_empty() {
                bail!("no station has {} — drop the need or add such a model", needs.join(" + "));
            }
            let at = ingress.and_then(|m| self.index(m)).filter(|i| eligible.contains(i)).unwrap_or_else(|| {
                eligible.iter().copied().min_by_key(|&i| (st.queues[i].backlog(labels.activity), i)).unwrap_or(eligible[0])
            });
            st.queues[at].waiting.entry(labels.activity).or_default().push_back(id);
            let mut task = Task::new(id, labels, brief, at, false, None, Some(tx));
            task.needs = needs;
            task.writes = writes;
            task.from_user = from_user;
            st.tasks.insert(id, task);
        }
        self.pump();
        Ok((self.receipt(id), rx))
    }

    /// A follow-up with a finished task's subagent: its transcript resumes
    /// at the station that last ran it, no routing. The new brief has its
    /// own labels and write areas.
    pub fn follow_up(
        self: &Arc<Self>,
        id: u32,
        labels: Labels,
        brief: String,
        writes: Vec<String>,
        resume: (Vec<Message>, u32),
        from_user: bool,
    ) -> Result<(Receipt, oneshot::Receiver<Finished>)> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.state.lock().unwrap();
            if st.tasks.contains_key(&id) {
                bail!("agent-{id} is still working — follow up after its report");
            }
            let Some(&at) = st.history.get(&id) else {
                bail!("agent-{id} did not run in the pool this session");
            };
            st.queues[at].waiting.entry(labels.activity).or_default().push_back(id);
            let mut task = Task::new(id, labels, brief, at, true, Some(resume), Some(tx));
            task.writes = writes;
            task.from_user = from_user;
            st.tasks.insert(id, task);
        }
        self.pump();
        Ok((self.receipt(id), rx))
    }

    fn receipt(&self, id: u32) -> Receipt {
        let st = self.state.lock().unwrap();
        let Some(task) = st.tasks.get(&id) else {
            return Receipt { model: String::new(), ahead: None };
        };
        let q = &st.queues[task.at];
        let ahead = q.waiting.values().flatten().position(|x| *x == id);
        Receipt { model: self.stations[task.at].model.clone(), ahead }
    }

    /// Every task in the pool, oldest first.
    pub fn snapshot(&self) -> Vec<Live> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<Live> = st
            .tasks
            .values()
            .map(|t| {
                let queue: Vec<u32> = st.queues[t.at].waiting.values().flatten().copied().collect();
                let state = match queue.iter().position(|x| *x == t.id) {
                    Some(ahead) => LiveState::Queued(ahead),
                    None if !t.waiting_for.is_empty() => LiveState::Waiting(t.waiting_for.iter().copied().collect()),
                    None => LiveState::Running,
                };
                Live {
                    id: t.id,
                    parent: t.parent,
                    labels: t.labels,
                    title: t.brief.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(80).collect(),
                    writes: t.writes.clone(),
                    model: self.stations[t.at].model.clone(),
                    state,
                    secs: t.started.elapsed().as_secs(),
                    splits: t.splits,
                }
            })
            .collect();
        out.sort_by_key(|l| l.id);
        out
    }

    /// (running, queued) across the pool.
    pub fn load(&self) -> (u32, usize) {
        let st = self.state.lock().unwrap();
        (st.running, st.queues.iter().map(Queues::queued).sum())
    }

    /// Dispatch everything that can move (paper: Dispatch + ProcessTask's
    /// routing step), then start the runs outside the lock.
    fn pump(self: &Arc<Self>) {
        let mut runs = Vec::new();
        {
            let mut guard = self.state.lock().unwrap();
            let st = &mut *guard;
            let expertise = self.expertise.lock().unwrap();
            let mut rng = self.rng.lock().unwrap();
            loop {
                let mut moved = false;
                for i in 0..self.stations.len() {
                    while let Some(step) = self.step(st, i, &expertise, &mut rng) {
                        moved = true;
                        if let Step::Run(i, id) = step {
                            runs.push((i, id));
                        }
                    }
                }
                if !moved {
                    break;
                }
            }
        }
        for (i, id) in runs {
            let me = self.clone();
            tokio::spawn(async move { me.run(i, id).await });
        }
    }

    fn step(&self, st: &mut State, i: usize, expertise: &Expertise, rng: &mut Rng) -> Option<Step> {
        if st.running >= self.config.max_running || st.queues[i].busy() >= self.capacity(i) {
            return None;
        }
        let t = st.queues[i].longest()?;
        let id = st.queues[i].waiting.get_mut(&t)?.pop_front()?;
        *st.queues[i].in_flight.entry(t).or_default() += 1;
        let (pinned, prev, hops, needs, labels, mut draws, avoid, no_explore, covered) = {
            let task = st.tasks.get_mut(&id)?;
            let covered = std::mem::take(&mut task.covered);
            (task.pinned || covered, task.prev, task.hops, task.needs.clone(), task.labels, std::mem::take(&mut task.draws), task.avoid.take(), task.no_explore, covered)
        };
        // A continuation that ran out on a station still being explored
        // leaves it, whatever the scores and hops say, when anyone else can
        // take it. A task that has run out once goes by the estimates from
        // then on, not by exploring again, since it has already lost time.
        let must_move = avoid == Some(i);
        let explore = self.config.explore && !no_explore;
        // Coverage (training runs): a fresh task goes to the least-tried
        // eligible station until every one has `explore_min` outcomes.
        if self.config.explore_min > 0 && explore && !pinned && !covered && hops == 0 && !must_move {
            let tried = |j: usize| expertise.outcomes(&self.stations[j].model);
            let floor = self.config.explore_min as usize;
            let least = (0..self.stations.len())
                .filter(|&j| self.stations[j].can(&needs) && tried(j) < floor && (j == i || !self.on_probation(st, j, expertise)))
                .min_by_key(|&j| (tried(j), j));
            if let Some(j) = least {
                st.tasks.get_mut(&id)?.draws = draws;
                if j != i {
                    *st.queues[i].in_flight.get_mut(&t)? -= 1;
                    st.queues[j].waiting.entry(t).or_default().push_back(id);
                    let task = st.tasks.get_mut(&id)?;
                    task.prev = Some(i);
                    task.hops += 1;
                    task.at = j;
                    task.covered = true;
                    tracing::info!(agent = id, task = %labels.name(), to = short_model(&self.stations[j].model), outcomes = tried(j), "deals: coverage, forwarded to the least-tried station");
                    return Some(Step::Forwarded);
                }
                st.running += 1;
                let task = st.tasks.get_mut(&id)?;
                if !task.executors.contains(&i) {
                    task.executors.push(i);
                }
                return Some(Step::Run(i, id));
            }
        }
        if !pinned && (hops < self.config.hops || must_move) {
            let costs = expertise.cost_model(|m| self.stations.iter().find(|s| s.model == m).map(|s| s.cost_guess()));
            let here = self.ad(st, i, &labels, expertise, rng, &mut draws, &costs, explore);
            let neighbors: Vec<(usize, Ad)> = (0..self.stations.len())
                .filter(|&j| j != i && Some(j) != prev && self.stations[j].can(&needs) && !self.on_probation(st, j, expertise))
                .map(|j| (j, self.ad(st, j, &labels, expertise, rng, &mut draws, &costs, explore)))
                .collect();
            let rivals: Vec<Ad> = prev
                .filter(|&j| self.stations[j].can(&needs))
                .map(|j| self.ad(st, j, &labels, expertise, rng, &mut draws, &costs, explore))
                .into_iter()
                .collect();
            st.tasks.get_mut(&id)?.draws = draws;
            if let Some(j) = router::choose(&here, &neighbors, &rivals, self.config.tolerance, !must_move) {
                *st.queues[i].in_flight.get_mut(&t)? -= 1;
                st.queues[j].waiting.entry(t).or_default().push_back(id);
                let task = st.tasks.get_mut(&id)?;
                task.prev = Some(i);
                task.hops += 1;
                task.at = j;
                tracing::info!(agent = id, task = %labels.name(), from = short_model(&self.stations[i].model), to = short_model(&self.stations[j].model), "deals: forwarded");
                return Some(Step::Forwarded);
            }
        }
        st.running += 1;
        let task = st.tasks.get_mut(&id)?;
        if !task.executors.contains(&i) {
            task.executors.push(i);
        }
        Some(Step::Run(i, id))
    }

    /// Up to k similar successes from this station's memory.
    async fn demonstrations(&self, i: usize, id: u32) -> String {
        let (Some(memory), Some(_)) = (&self.memory, &self.embedder) else { return String::new() };
        let pool = memory.load(&self.stations[i].model);
        if pool.is_empty() {
            return String::new();
        }
        let Some(query) = self.embedding(id).await else { return String::new() };
        let demos = super::memory::retrieve(&query, &pool, self.config.memory_k, self.config.memory_theta);
        super::memory::render(&demos)
    }

    /// The task's brief embedding, computed once.
    async fn embedding(&self, id: u32) -> Option<Vec<f32>> {
        let brief = {
            let st = self.state.lock().unwrap();
            let task = st.tasks.get(&id)?;
            if let Some(e) = &task.embedding {
                return Some(e.clone());
            }
            task.brief.clone()
        };
        match self.embedder.as_ref()?.embed(&brief).await {
            Ok(e) => {
                if let Some(task) = self.state.lock().unwrap().tasks.get_mut(&id) {
                    task.embedding = Some(e.clone());
                }
                Some(e)
            }
            Err(e) => {
                tracing::warn!("deals: embedding failed, no demonstrations: {e:#}");
                None
            }
        }
    }

    async fn run(self: Arc<Self>, i: usize, id: u32) {
        let demos = self.demonstrations(i, id).await;
        let segment = {
            let mut st = self.state.lock().unwrap();
            let Some(task) = st.tasks.get_mut(&id) else { return };
            Segment {
                task: id,
                model: self.stations[i].model.clone(),
                labels: task.labels,
                title: task.brief.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(120).collect(),
                brief: format!("{demos}{}", task.composed_brief()),
                continued: task.pinned || !task.parts.is_empty(),
                resume: task.resume.take(),
                writes: task.writes.clone(),
                can_fork: self.config.fork && task.depth < self.config.fork_depth,
                time_limit: self.time_limit(task.labels.difficulty),
                from_user: task.from_user,
            }
        };
        let t = segment.labels.activity;
        let result = (self.exec)(segment).await;
        let finished = self.settle(i, id, t, result);
        if let Some(task) = finished {
            let me = self.clone();
            tokio::spawn(async move { me.finish(task).await });
        }
        self.pump();
    }

    /// Book the segment's end. Returns the task when it leaves the pool.
    fn settle(&self, i: usize, id: u32, t: Key, result: SegmentResult) -> Option<(Task, Ending, String)> {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        if let Some(n) = st.queues[i].in_flight.get_mut(&t) {
            *n = n.saturating_sub(1);
        }
        st.running = st.running.saturating_sub(1);
        let model = self.stations[i].model.clone();
        let task = st.tasks.get_mut(&id)?;
        *task.cost.entry(model.clone()).or_default() += result.cost;
        let (commands, mut files) = qa::trace(&result.transcript);
        files.extend(result.files.iter().cloned());
        task.commands.extend(commands);
        for f in files {
            if !task.files.contains(&f) {
                task.files.push(f);
            }
        }
        task.segments.push((i, steps(&result.transcript), result.secs));
        let can_split = task.splits < self.config.splits;
        let (ending, report) = match result.end {
            SegmentEnd::Split { done, remaining } if can_split => {
                task.split(done, remaining);
                st.queues[i].waiting.entry(t).or_default().push_back(id);
                tracing::info!(agent = id, at = short_model(&model), "deals: split, continuation queued");
                return None;
            }
            SegmentEnd::Fork { done, children, then } if can_split => {
                let allowed = self.config.fork && task.depth < self.config.fork_depth && (2..=self.config.fork_width as usize).contains(&children.len());
                if !allowed {
                    // Not here: the subtasks become this task's remaining work, in order.
                    let listed: String = children.iter().enumerate().map(|(n, c)| format!("\n{}. {}", n + 1, c.brief)).collect();
                    task.split(done, format!("{then}\nDo these first, in order:{listed}"));
                    st.queues[i].waiting.entry(t).or_default().push_back(id);
                    return None;
                }
                task.split(done, then);
                let depth = task.depth + 1;
                let ids: Vec<u32> = children.iter().map(|_| self.ids.fetch_add(1, Ordering::Relaxed)).collect();
                task.waiting_for = ids.iter().copied().collect();
                for (cid, spec) in ids.iter().zip(children) {
                    let mut child = Task::new(*cid, spec.labels, spec.brief, i, false, None, None);
                    child.parent = Some(id);
                    child.depth = depth;
                    child.writes = spec.writes;
                    st.queues[i].waiting.entry(spec.labels.activity).or_default().push_back(*cid);
                    st.tasks.insert(*cid, child);
                }
                tracing::info!(agent = id, subtasks = ?ids, at = short_model(&model), "deals: forked");
                return None;
            }
            SegmentEnd::TurnLimit(last) if can_split => {
                // Learned now, not when the task ends: a run that is cut off
                // later (the whole process killed) still teaches. This station
                // gets a weak failure and leaves the task's final credit.
                let partial = Outcome {
                    ts: chrono::Utc::now(),
                    project: self.project.clone(),
                    labels: task.labels,
                    stations: vec![model.clone()],
                    success: Ending::TurnLimit.prior(),
                    cost: BTreeMap::from([(model.clone(), task.cost.remove(&model).unwrap_or(0.0))]),
                    secs: result.secs,
                    partial: true,
                };
                task.executors.retain(|&x| x != i);
                task.draws.remove(&i);
                // Running out on a station still being explored likely means
                // it is slow or struggling: move. An established one may just
                // have a long task: routing decides, with its estimate lowered.
                let mut expertise = self.expertise.lock().unwrap();
                let established = expertise.outcomes(&model) >= (3 * self.config.probation).max(3) as usize;
                task.avoid = (!established).then_some(i);
                task.no_explore = true;
                task.split(last, "The previous worker ran out of turns or time before finishing; finish the brief.".into());
                st.queues[i].waiting.entry(t).or_default().push_back(id);
                tracing::info!(agent = id, at = short_model(&model), moves = !established, "deals: turn or time limit, continuation queued");
                expertise.record(partial);
                return None;
            }
            SegmentEnd::Done(r) => (Ending::Done, r),
            SegmentEnd::Split { done, remaining } => (Ending::TurnLimit, format!("{done}\n\nNot finished: {remaining}")),
            SegmentEnd::Fork { done, children, then } => {
                let listed: String = children.iter().map(|c| format!("\n- {}", c.brief.lines().next().unwrap_or(""))).collect();
                (Ending::TurnLimit, format!("{done}\n\nNot finished (split limit reached before this fork):{listed}\nThen: {then}"))
            }
            SegmentEnd::TurnLimit(r) => (Ending::TurnLimit, r),
            SegmentEnd::Failed(e) => (Ending::Failed, e),
            SegmentEnd::Interrupted(r) => (Ending::Interrupted, r),
        };
        let task = st.tasks.remove(&id)?;
        st.history.insert(id, i);
        Some((task, ending, report))
    }

    /// Judge, learn, remember, report (paper: Finish).
    async fn finish(self: Arc<Self>, (mut task, ending, report): (Task, Ending, String)) {
        let full_report = if task.parts.is_empty() {
            report
        } else {
            let earlier: String = task.parts.iter().enumerate().map(|(n, p)| format!("{}. {p}\n", n + 1)).collect();
            format!("Finished in earlier segments:\n{earlier}\n{report}")
        };
        let evidence = Evidence {
            activity: task.labels.activity,
            read_only: task.writes.is_empty(),
            brief: task.brief.clone(),
            ending,
            report: full_report.clone(),
            commands: task.commands.clone(),
            files: task.files.clone(),
            project: std::path::PathBuf::from(&self.project),
        };
        let p = if self.config.qa { qa::judge(self.decider.as_deref(), &evidence).await } else { ending.prior() };
        let secs = task.started.elapsed().as_secs();
        let stations: Vec<String> = task.executors.iter().map(|&i| self.stations[i].model.clone()).collect();
        if ending != Ending::Interrupted {
            self.expertise.lock().unwrap().record(Outcome {
                ts: chrono::Utc::now(),
                project: self.project.clone(),
                labels: task.labels,
                stations: stations.clone(),
                success: p,
                cost: task.cost.clone(),
                secs,
                partial: false,
            });
        }
        if p >= 0.7 && ending == Ending::Done {
            if let Some(memory) = &self.memory {
                let embedding = match task.embedding.take() {
                    Some(e) => Some(e),
                    None => match &self.embedder {
                        Some(e) => e.embed(&task.brief).await.ok(),
                        None => None,
                    },
                };
                if let Some(embedding) = embedding {
                    for (i, steps, secs) in &task.segments {
                        let t = Trajectory {
                            ts: chrono::Utc::now(),
                            labels: task.labels,
                            brief: task.brief.clone(),
                            steps: steps.clone(),
                            report: full_report.clone(),
                            secs: *secs,
                            embedding: embedding.clone(),
                        };
                        if let Err(e) = memory.add(&self.stations[*i].model, t) {
                            tracing::warn!("deals: memory write failed: {e:#}");
                        }
                    }
                }
            }
        }
        let last = task.segments.last().map(|s| s.0).unwrap_or(task.at);
        tracing::info!(
            agent = task.id,
            task = %task.labels.name(),
            stations = ?stations.iter().map(|s| short_model(s)).collect::<Vec<_>>(),
            qa = p,
            difficulty = task.labels.difficulty.unwrap_or(-1.0),
            hops = task.hops,
            splits = task.splits,
            secs,
            "deals: finished"
        );
        if let Some(parent) = task.parent {
            let files = if task.files.is_empty() { String::new() } else { format!("\nFiles: {}", task.files.join(", ")) };
            let result = format!(
                "[{} agent-{}, QA {p:.2}{}] {}\n{}{files}",
                task.labels.name(),
                task.id,
                if ending == Ending::Done { "" } else { ", unfinished" },
                task.brief.lines().next().unwrap_or("").chars().take(160).collect::<String>(),
                clip(&full_report, 2400),
            );
            self.join(parent, task.id, result, task.files.clone());
            self.pump();
            return;
        }
        if let Some(done) = task.done.take() {
            let _ = done.send(Finished {
                report: full_report,
                model: self.stations[last].model.clone(),
                qa: p,
                secs,
                splits: task.splits,
                ok: ending == Ending::Done,
            });
        }
    }
}

impl Pool {
    /// A subtask finished: its result joins the parent's; the last one
    /// queues the parent's continuation.
    fn join(&self, parent: u32, child: u32, result: String, files: Vec<String>) {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        let Some(p) = st.tasks.get_mut(&parent) else { return };
        p.parts.push(result);
        for f in files {
            if !p.files.contains(&f) {
                p.files.push(f);
            }
        }
        p.waiting_for.remove(&child);
        if p.waiting_for.is_empty() {
            let (at, t) = (p.at, p.labels.activity);
            st.queues[at].waiting.entry(t).or_default().push_back(parent);
            tracing::info!(agent = parent, "deals: fork joined, continuation queued");
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push_str(" …");
    out
}

impl Task {
    fn new(
        id: u32,
        labels: Labels,
        brief: String,
        at: usize,
        pinned: bool,
        resume: Option<(Vec<Message>, u32)>,
        done: Option<oneshot::Sender<Finished>>,
    ) -> Task {
        Task {
            id,
            labels,
            brief,
            needs: Vec::new(),
            hops: 0,
            splits: 0,
            prev: None,
            at,
            pinned,
            resume,
            executors: Vec::new(),
            parts: Vec::new(),
            remaining: None,
            cost: BTreeMap::new(),
            commands: Vec::new(),
            files: Vec::new(),
            segments: Vec::new(),
            embedding: None,
            started: Instant::now(),
            done,
            parent: None,
            depth: 0,
            writes: Vec::new(),
            waiting_for: std::collections::BTreeSet::new(),
            draws: HashMap::new(),
            avoid: None,
            no_explore: false,
            from_user: false,
            covered: false,
        }
    }

    fn split(&mut self, done: String, remaining: String) {
        self.splits += 1;
        self.parts.push(done);
        self.remaining = Some(remaining);
        // A continuation is a fresh subagent: the next station may be a
        // different model, so it gets the results, not the transcript.
        self.pinned = false;
    }

    /// The brief, plus what earlier segments finished (paper: "Completed
    /// Subtasks and Results").
    fn composed_brief(&self) -> String {
        if self.parts.is_empty() {
            return self.brief.clone();
        }
        let mut out = format!("{}\n\n[continuation] Earlier workers finished part of this brief (parallel subtasks report with their QA score — an automatic judge's confidence; check low ones). Their results:\n", self.brief);
        for (n, p) in self.parts.iter().enumerate() {
            out.push_str(&format!("{}. {p}\n", n + 1));
        }
        if let Some(r) = &self.remaining {
            out.push_str(&format!("What remains: {r}\n"));
        }
        out.push_str("Do not redo the finished parts; check them only as far as you need to continue.");
        out
    }
}

/// One line per tool call — the trajectory a demonstration shows.
fn steps(transcript: &[Message]) -> String {
    let mut lines = Vec::new();
    for m in transcript {
        if let Message::Assistant { tool_calls, .. } = m {
            for c in tool_calls {
                lines.push(format!("- {}", crate::tools::summarize(c)));
            }
        }
    }
    if lines.len() > 40 {
        let skipped = lines.len() - 40;
        lines.drain(20..20 + skipped);
        lines.insert(20, format!("- … {skipped} more steps"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::catalog::Prices;
    use super::super::{Activity, Activity::*, Domain, Domain::*};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn station(name: &str, price: f64) -> Station {
        Station {
            model: format!("test/{name}"),
            context: 200_000,
            prices: Prices { input: price, output: price, cached_input: Some(price) },
            paid: false,
            reasoning: false,
            caps: vec!["function_calling".into()],
            probe: None,
        }
    }

    fn l(a: Activity, d: Domain) -> Labels {
        Labels { activity: Some(a), domain: Some(d), difficulty: None }
    }

    fn at(a: Activity, d: Domain, difficulty: f64) -> Labels {
        Labels { difficulty: Some(difficulty), ..l(a, d) }
    }

    fn outcome(station: &str, labels: Labels, success: f64) -> Outcome {
        Outcome { ts: chrono::Utc::now(), project: "p".into(), labels, stations: vec![station.into()], success, cost: BTreeMap::new(), secs: 1, partial: false }
    }

    /// Deterministic routing (no exploration) unless a test asks for it.
    fn config(slots: u32) -> DealsConfig {
        DealsConfig { enabled: true, slots, qa: false, probation: 0, explore: false, ..DealsConfig::default() }
    }

    type Script = Arc<dyn Fn(&Segment) -> SegmentEnd + Send + Sync>;

    /// Runs each segment for `ms`, records which model ran it.
    fn exec(ms: u64, ran: Arc<Mutex<Vec<(u32, String)>>>, peak: Arc<AtomicU32>, script: Script) -> Exec {
        let live = Arc::new(AtomicU32::new(0));
        Arc::new(move |seg: Segment| {
            let (ran, peak, live, script) = (ran.clone(), peak.clone(), live.clone(), script.clone());
            Box::pin(async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                ran.lock().unwrap().push((seg.task, seg.model.clone()));
                tokio::time::sleep(Duration::from_millis(ms)).await;
                live.fetch_sub(1, Ordering::SeqCst);
                SegmentResult { end: script(&seg), cost: 0.01, secs: 1, transcript: vec![], files: vec![] }
            })
        })
    }

    fn done() -> Script {
        Arc::new(|s: &Segment| SegmentEnd::Done(format!("did {}", s.task)))
    }

    fn pool(cfg: DealsConfig, stations: Vec<Station>, expertise: Expertise, exec: Exec) -> Arc<Pool> {
        Arc::new(Pool::new(cfg, stations, expertise, None, None, None, exec, "test".into(), Arc::new(AtomicU32::new(100))))
    }

    #[tokio::test]
    async fn backlog_spreads_a_burst_across_idle_stations() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let p = pool(config(1), vec![station("a", 1.0), station("b", 1.0), station("c", 1.0)], Expertise::load(None), exec(40, ran.clone(), peak.clone(), done()));
        let mut rxs = vec![];
        for id in 1..=6 {
            rxs.push(p.submit(id, l(New, Backend), format!("task {id}"), vec![], vec![], Some("test/a"), false).unwrap().1);
        }
        for rx in rxs {
            assert!(rx.await.unwrap().ok);
        }
        let models: std::collections::HashSet<String> = ran.lock().unwrap().iter().map(|r| r.1.clone()).collect();
        assert_eq!(models.len(), 3, "all three stations served the burst: {models:?}");
        assert_eq!(peak.load(Ordering::SeqCst), 3, "one slot each, all busy at once");
    }

    #[tokio::test]
    async fn expertise_pulls_work_to_the_station_that_succeeds() {
        let mut exp = Expertise::load(None);
        for _ in 0..10 {
            exp.record(outcome("test/strong", l(Debug, Backend), 1.0));
            exp.record(outcome("test/weak", l(Debug, Backend), 0.0));
        }
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let cfg = config(4);
        let p = pool(cfg, vec![station("weak", 1.0), station("strong", 1.0)], exp, exec(20, ran.clone(), peak, done()));
        let mut rxs = vec![];
        for id in 1..=4 {
            rxs.push(p.submit(id, l(Debug, Backend), "fix it".into(), vec![], vec![], Some("test/weak"), false).unwrap().1);
        }
        for rx in rxs {
            assert_eq!(rx.await.unwrap().model, "test/strong");
        }
        // Work neither has tried: each starts from its overall ability, so
        // the strong station is still the better bet.
        let (_, rx) = p.submit(9, l(Writing, Prose), "write docs".into(), vec![], vec![], Some("test/weak"), false).unwrap();
        assert_eq!(rx.await.unwrap().model, "test/strong");
    }

    #[tokio::test]
    async fn cost_keeps_work_off_an_expensive_equal() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let cfg = config(9);
        let p = pool(cfg, vec![station("cheap", 0.1), station("pricey", 3.0)], Expertise::load(None), exec(5, ran.clone(), peak, done()));
        // Lands at the expensive station; the cheap one is equally good and idle.
        let (_, rx) = p.submit(1, l(New, Backend), "x".into(), vec![], vec![], Some("test/pricey"), false).unwrap();
        assert_eq!(rx.await.unwrap().model, "test/cheap");
    }

    #[tokio::test]
    async fn an_untried_cheap_station_gets_one_task_at_a_time() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let mut exp = Expertise::load(None);
        // Even odds, as an untried station starts, so the cheap one is as good a bet.
        for _ in 0..5 {
            exp.record(outcome("test/known", l(New, Backend), 0.5));
        }
        let cfg = DealsConfig { probation: 3, ..config(9) };
        let p = pool(cfg, vec![station("known", 1.0), station("tiny", 0.01)], exp, exec(30, ran.clone(), peak, done()));
        let mut rxs = vec![];
        for id in 1..=5 {
            rxs.push(p.submit(id, l(New, Backend), format!("t{id}"), vec![], vec![], Some("test/known"), false).unwrap().1);
        }
        for rx in rxs {
            rx.await.unwrap();
        }
        let first_wave: Vec<String> = ran.lock().unwrap().iter().take(5).map(|r| r.1.clone()).collect();
        assert_eq!(first_wave.iter().filter(|m| *m == "test/tiny").count(), 1, "one probe task, not the burst: {first_wave:?}");
    }

    #[tokio::test]
    async fn exploring_tries_untried_stations_but_not_a_proven_bad_one() {
        // One station known good, one known bad, two never tried; all priced alike.
        let mut exp = Expertise::load(None);
        for _ in 0..20 {
            exp.record(outcome("test/good", l(New, Backend), 0.9));
            exp.record(outcome("test/bad", l(New, Backend), 0.0));
        }
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let cfg = DealsConfig { explore: true, ..config(9) };
        let stations = vec![station("good", 1.0), station("bad", 1.0), station("new1", 1.0), station("new2", 1.0)];
        let p = pool(cfg, stations, exp, exec(1, ran.clone(), peak, done()));
        *p.rng.lock().unwrap() = Rng::new(11);
        for id in 1..=40 {
            let (_, rx) = p.submit(id, l(New, Backend), format!("t{id}"), vec![], vec![], Some("test/good"), false).unwrap();
            rx.await.unwrap();
        }
        let by: std::collections::HashMap<String, usize> = ran.lock().unwrap().iter().fold(Default::default(), |mut m, (_, model)| {
            *m.entry(model.clone()).or_default() += 1;
            m
        });
        let n = |m: &str| by.get(&format!("test/{m}")).copied().unwrap_or(0);
        assert!(n("new1") + n("new2") >= 2, "untried stations get tried: {by:?}");
        assert_eq!(n("bad"), 0, "a station that has shown it fails is left alone: {by:?}");
        assert!(n("good") >= 10, "the known good one still does most: {by:?}");
    }

    #[tokio::test]
    async fn coverage_sends_tasks_to_the_least_tried_until_each_has_enough() {
        let mut exp = Expertise::load(None);
        for _ in 0..10 {
            exp.record(outcome("test/known", l(New, Backend), 0.9));
        }
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let cfg = DealsConfig { explore: true, explore_min: 2, probation: 3, ..config(9) };
        let stations = vec![station("known", 0.1), station("dear1", 5.0), station("dear2", 5.0)];
        let p = pool(cfg, stations, exp, exec(1, ran.clone(), peak, done()));
        for id in 1..=6 {
            let (_, rx) = p.submit(id, l(New, Backend), format!("t{id}"), vec![], vec![], Some("test/known"), false).unwrap();
            rx.await.unwrap();
        }
        let models: Vec<String> = ran.lock().unwrap().iter().map(|r| r.1.clone()).collect();
        // Expensive untried stations get their two each first, then the rest by their draws.
        assert_eq!(models[..4].iter().filter(|m| m.as_str() != "test/known").count(), 4, "{models:?}");
        assert!(models.iter().filter(|m| *m == "test/dear1").count() >= 2, "{models:?}");
        assert!(models.iter().filter(|m| *m == "test/dear2").count() >= 2, "{models:?}");
    }

    #[tokio::test]
    async fn needs_restrict_routing_to_capable_stations() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let mut eyes = station("eyes", 1.0);
        eyes.caps.push("vision".into());
        let p = pool(config(9), vec![station("blind", 0.01), eyes], Expertise::load(None), exec(5, ran, peak, done()));
        // Lands at the cheap blind station by request, but only eyes can see.
        let (_, rx) = p.submit(1, l(Review, Frontend), "look at shot.png".into(), vec!["vision".into()], vec![], Some("test/blind"), false).unwrap();
        assert_eq!(rx.await.unwrap().model, "test/eyes");
        assert!(p.submit(2, l(Review, Backend), "x".into(), vec!["long_context".into()], vec![], None, false).is_err(), "nobody has 500k");
        assert!(p.submit(3, l(Review, Backend), "x".into(), vec!["telepathy".into()], vec![], None, false).is_err());
    }

    fn spec(labels: Labels, brief: &str, writes: &[&str]) -> ChildSpec {
        ChildSpec { labels, brief: brief.into(), writes: writes.iter().map(|w| w.to_string()).collect() }
    }

    #[tokio::test]
    async fn a_fork_runs_its_subtasks_in_parallel_and_joins_their_reports() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|s: &Segment| {
            if s.task == 1 && !s.brief.contains("[continuation]") {
                assert!(s.can_fork, "a filed task may fork");
                SegmentEnd::Fork {
                    done: "read the layout".into(),
                    children: vec![
                        spec(l(Research, Systems), "how vectorized emulation keeps lanes", &[]),
                        spec(l(Research, Binary), "how gzip reads its input", &[]),
                        spec(l(New, Systems), "scaffold the RV32 decoder", &["src/decode"]),
                    ],
                    then: "build the lane engine on these".into(),
                }
            } else if s.task == 1 {
                for part in ["read the layout", "[research·systems agent-", "[research·binary agent-", "[new·systems agent-", "What remains: build the lane engine"] {
                    assert!(s.brief.contains(part), "continuation lacks {part:?}: {}", s.brief);
                }
                SegmentEnd::Done("engine built".into())
            } else {
                assert!(!s.can_fork, "subtasks don't fork again at depth 1");
                if s.labels.activity == Some(New) {
                    assert_eq!(s.writes, vec!["src/decode"]);
                } else {
                    assert!(s.writes.is_empty(), "readers stay read-only");
                }
                SegmentEnd::Done(format!("did subtask {}", s.task))
            }
        });
        let p = pool(config(4), vec![station("a", 1.0), station("b", 1.0)], Expertise::load(None), exec(30, ran.clone(), peak.clone(), script));
        let (_, rx) = p.submit(1, l(New, Systems), "make a fuzzer".into(), vec![], vec![".".into()], None, false).unwrap();
        let f = rx.await.unwrap();
        assert!(f.ok && f.report.contains("engine built") && f.report.contains("did subtask"), "{}", f.report);
        assert_eq!(ran.lock().unwrap().len(), 5, "parent, three subtasks, continuation");
        assert!(peak.load(Ordering::SeqCst) >= 3, "the subtasks ran at the same time");
        let exp = p.expertise.lock().unwrap();
        let binary: usize = ["test/a", "test/b"]
            .iter()
            .filter_map(|m| exp.facets(m))
            .flat_map(|(_, f)| f.into_iter().filter(|(name, _, _)| *name == "binary").map(|(_, _, n)| n))
            .sum();
        assert_eq!(binary, 1, "each subtask is learned from under its own labels");
    }

    #[tokio::test]
    async fn a_fork_past_the_nesting_limit_continues_in_order() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|s: &Segment| {
            if s.brief.contains("Do these first, in order") {
                SegmentEnd::Done("did them in order".into())
            } else {
                SegmentEnd::Fork {
                    done: String::new(),
                    children: vec![spec(l(Writing, Prose), "document a", &["a.md"]), spec(l(Writing, Prose), "document b", &["b.md"])],
                    then: "check both".into(),
                }
            }
        });
        let p = pool(DealsConfig { fork_depth: 0, ..config(2) }, vec![station("a", 1.0)], Expertise::load(None), exec(1, ran.clone(), peak, script));
        let (_, rx) = p.submit(1, l(Writing, Prose), "document things".into(), vec![], vec![".".into()], None, false).unwrap();
        let f = rx.await.unwrap();
        assert!(f.report.contains("did them in order"));
        assert_eq!(ran.lock().unwrap().len(), 2, "no subtasks started");
    }

    #[tokio::test]
    async fn easy_work_goes_to_the_cheap_model_and_hard_work_to_the_strong_one() {
        let mut exp = Expertise::load(None);
        let o = |station: &str, success: f64, d: f64| outcome(station, at(New, Backend, d), success);
        for _ in 0..8 {
            exp.record(o("test/small", 1.0, 0.5));
            exp.record(o("test/small", 1.0, 1.5));
            exp.record(o("test/small", 0.0, 3.0));
            exp.record(o("test/big", 1.0, 1.0));
            exp.record(o("test/big", 1.0, 3.0));
        }
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let cfg = config(9);
        // small is 20x cheaper; both enter at big.
        let p = pool(cfg, vec![station("big", 2.0), station("small", 0.1)], exp, exec(5, ran, peak, done()));
        let (_, easy) = p.submit(1, at(New, Backend, 0.5), "rename a variable".into(), vec![], vec![], Some("test/big"), false).unwrap();
        assert_eq!(easy.await.unwrap().model, "test/small", "easy: the cheap model is as good");
        let (_, hard) = p.submit(2, at(New, Backend, 3.2), "fix the race".into(), vec![], vec![], Some("test/small"), false).unwrap();
        assert_eq!(hard.await.unwrap().model, "test/big", "hard: only the strong model is likely to succeed");
    }

    #[tokio::test]
    async fn a_split_continues_as_a_fresh_segment_with_the_finished_part() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|s: &Segment| {
            if s.brief.contains("[continuation]") {
                assert!(s.brief.contains("1. parsed the config") && s.brief.contains("What remains: wire it up"));
                SegmentEnd::Done("wired".into())
            } else {
                SegmentEnd::Split { done: "parsed the config".into(), remaining: "wire it up".into() }
            }
        });
        let p = pool(config(2), vec![station("a", 1.0)], Expertise::load(None), exec(5, ran.clone(), peak, script));
        let (_, rx) = p.submit(1, l(Change, Backend), "parse and wire".into(), vec![], vec![".".into()], None, false).unwrap();
        let f = rx.await.unwrap();
        assert!(f.ok && f.splits == 1);
        assert!(f.report.contains("parsed the config") && f.report.contains("wired"));
        assert_eq!(ran.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_station_that_runs_out_is_learned_from_at_once_and_the_task_moves_on() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|s: &Segment| {
            if s.model == "test/slow" { SegmentEnd::TurnLimit("(ran out of time)\nhalf done".into()) } else { SegmentEnd::Done("finished".into()) }
        });
        // Backlog-only routing: nothing would move it off slow by score.
        let p = pool(config(2), vec![station("slow", 1.0), station("fast", 1.0)], Expertise::load(None), exec(1, ran.clone(), peak, script));
        let (_, rx) = p.submit(1, l(Analyze, Data), "add up the orders".into(), vec![], vec![".".into()], Some("test/slow"), false).unwrap();
        let f = rx.await.unwrap();
        assert!(f.ok && f.model == "test/fast", "the continuation ran elsewhere");
        let models: Vec<String> = ran.lock().unwrap().iter().map(|r| r.1.clone()).collect();
        assert_eq!(models, vec!["test/slow", "test/fast"]);
        let exp = p.expertise.lock().unwrap();
        assert_eq!(exp.outcomes("test/slow"), 1, "slow learned its weak failure once, not again at the end");
        assert!(exp.p("test/slow", &l(Analyze, Data)) < exp.p("test/fast", &l(Analyze, Data)));
    }

    #[tokio::test]
    async fn a_task_that_ran_out_stops_exploring() {
        // A known good station, plus untried cheap ones that would win draws.
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|s: &Segment| {
            if s.model == "test/slowpoke" { SegmentEnd::TurnLimit("(ran out of time)".into()) } else { SegmentEnd::Done("ok".into()) }
        });
        let cfg = DealsConfig { explore: true, probation: 3, ..config(4) };
        let stations = vec![station("slowpoke", 0.5), station("good", 1.0), station("cheap1", 0.2), station("cheap2", 0.2)];
        for seed in 1..=10 {
            let p = pool(cfg.clone(), stations.clone(), Expertise::load(None), exec(1, ran.clone(), peak.clone(), script.clone()));
            {
                let mut e = p.expertise.lock().unwrap();
                for _ in 0..15 {
                    e.record(outcome("test/good", l(Analyze, Finance), 0.9));
                }
            }
            *p.rng.lock().unwrap() = Rng::new(seed);
            ran.lock().unwrap().clear();
            let (_, rx) = p.submit(1, l(Analyze, Finance), "reconcile".into(), vec![], vec![], Some("test/slowpoke"), false).unwrap();
            let f = rx.await.unwrap();
            let models: Vec<String> = ran.lock().unwrap().iter().map(|r| r.1.clone()).collect();
            if models.first().map(String::as_str) == Some("test/slowpoke") {
                assert_eq!(f.model, "test/good", "seed {seed}: after running out it goes to the best estimate, not another try: {models:?}");
            }
        }
    }

    #[tokio::test]
    async fn an_established_station_that_runs_out_may_keep_the_task() {
        let mut exp = Expertise::load(None);
        for _ in 0..12 {
            exp.record(outcome("test/strong", l(Analyze, Data), 1.0));
            exp.record(outcome("test/weak", l(Analyze, Data), 0.0));
        }
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let first = Arc::new(AtomicU32::new(0));
        let script: Script = Arc::new(move |_s: &Segment| {
            if first.fetch_add(1, Ordering::SeqCst) == 0 { SegmentEnd::TurnLimit("(ran out of time)\nlong job".into()) } else { SegmentEnd::Done("finished".into()) }
        });
        let cfg = DealsConfig { probation: 3, ..config(2) };
        let p = pool(cfg, vec![station("strong", 1.0), station("weak", 0.1)], exp, exec(1, ran.clone(), peak, script));
        let (_, rx) = p.submit(1, l(Analyze, Data), "a long job".into(), vec![], vec![".".into()], Some("test/strong"), false).unwrap();
        let f = rx.await.unwrap();
        assert_eq!(f.model, "test/strong", "one weak failure doesn't hand a proven station's task to a cheap bad one");
        assert_eq!(ran.lock().unwrap().len(), 2);
    }

    #[test]
    fn segment_time_limits_scale_with_difficulty() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let p = pool(DealsConfig { segment_secs: 300, ..config(1) }, vec![station("a", 1.0)], Expertise::load(None), exec(1, ran, peak, done()));
        let secs = |d: Option<f64>| p.time_limit(d).unwrap().as_secs();
        assert_eq!((secs(None), secs(Some(2.0)), secs(Some(3.0)), secs(Some(1.0))), (300, 300, 600, 150));
        assert_eq!((secs(Some(0.0)), secs(Some(4.0))), (75, 1200));
        let p0 = pool(DealsConfig { segment_secs: 0, ..config(1) }, vec![station("a", 1.0)], Expertise::load(None), exec(1, Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)), done()));
        assert!(p0.time_limit(Some(3.0)).is_none());
    }

    #[tokio::test]
    async fn splits_stop_at_the_limit() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|_| SegmentEnd::TurnLimit("partway".into()));
        let p = pool(DealsConfig { splits: 2, ..config(1) }, vec![station("a", 1.0)], Expertise::load(None), exec(1, ran.clone(), peak, script));
        let (_, rx) = p.submit(1, l(Test, Backend), "never ends".into(), vec![], vec![".".into()], None, false).unwrap();
        let f = rx.await.unwrap();
        assert!(!f.ok && f.splits == 2);
        assert_eq!(ran.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn back_pressure_refuses_past_max_queued_and_slots_bound_concurrency() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let p = pool(DealsConfig { max_queued: 2, ..config(1) }, vec![station("a", 1.0)], Expertise::load(None), exec(30, ran, peak.clone(), done()));
        let (r1, rx1) = p.submit(1, l(Writing, Prose), "one".into(), vec![], vec![], None, false).unwrap();
        assert!(r1.ahead.is_none(), "started at once");
        let (r2, rx2) = p.submit(2, l(Writing, Prose), "two".into(), vec![], vec![], None, false).unwrap();
        assert_eq!(r2.ahead, Some(0));
        let (_, rx3) = p.submit(3, l(Writing, Prose), "three".into(), vec![], vec![], None, false).unwrap();
        let err = p.submit(4, l(Writing, Prose), "four".into(), vec![], vec![], None, false).err().unwrap();
        assert!(err.to_string().contains("back pressure"));
        assert_eq!(p.load(), (1, 2));
        // The lead's view: one running, two in line behind it.
        let states: Vec<(u32, String)> = p
            .snapshot()
            .iter()
            .map(|l| (l.id, match &l.state { LiveState::Running => "running".into(), LiveState::Queued(n) => format!("queued {n}"), LiveState::Waiting(_) => "waiting".into() }))
            .collect();
        assert_eq!(states, vec![(1, "running".into()), (2, "queued 0".into()), (3, "queued 1".into())]);
        for rx in [rx1, rx2, rx3] {
            rx.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn outcomes_teach_the_stations_that_ran_them() {
        let (ran, peak) = (Arc::new(Mutex::new(vec![])), Arc::new(AtomicU32::new(0)));
        let script: Script = Arc::new(|_| SegmentEnd::Failed("model call failed".into()));
        let p = pool(DealsConfig { qa: true, ..config(1) }, vec![station("a", 1.0)], Expertise::load(None), exec(1, ran, peak, script));
        let (_, rx) = p.submit(1, l(Review, Backend), "look".into(), vec![], vec![], None, false).unwrap();
        assert!(!rx.await.unwrap().ok);
        assert!(p.expertise.lock().unwrap().p("test/a", &l(Review, Backend)) < 0.5);
        // Follow-ups go back to the station that ran it.
        let (_, rx) = p.follow_up(1, l(Debug, Backend), "now fix it".into(), vec![".".into()], (vec![], 1), false).unwrap();
        assert_eq!(rx.await.unwrap().model, "test/a");
        assert!(p.follow_up(7, l(Review, Backend), "never ran".into(), vec![], (vec![], 1), false).is_err());
    }
}
