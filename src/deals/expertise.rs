//! Per-station success and cost estimates (§5.8), learned from judged
//! outcomes. Each finished task appends one line to
//! `~/.tursi/deals/outcomes.jsonl`; the estimates are a fold over that log,
//! so concurrent tursi processes never clobber each other and the history
//! stays inspectable. The paper freezes its estimates after a warm-up split;
//! tursi keeps learning, because its models and its work keep changing.
//!
//! The paper learns one success rate per (station, task type). tursi's tasks
//! carry three labels instead (`labels`), and success is an additive
//! item-response model over them:
//!
//! logit P(success) = θ[station] + α[station, activity] + β[station, domain] − κ[station]·(d − 2)
//!
//! with d the task's difficulty on the 0–4 scale (2 is moderate) and κ the
//! station's logits lost per level. θ is the station's ability overall; α and
//! β are how much better or worse it does at an activity or in a domain, each
//! shrunk toward zero by its prior until outcomes say otherwise; κ is shrunk
//! toward a shared slope the same way, so a small model that is reliable on
//! easy work and collapses on hard work learns a steep slope instead of a
//! middling ability everywhere. So a station strong everywhere
//! starts strong at work it hasn't tried, an outcome in analyze·finance
//! teaches something about analyze and about finance, and every outcome,
//! easy or hard, sharpens where the station's limit lies. All three are
//! fitted together (MAP); judged probabilities are fractional observations.
//!
//! Training runs route on samples from these estimates instead of the
//! estimates themselves (Thompson sampling, not in the paper, which explores
//! only in its warm-up split): each parameter's posterior is approximated as normal around its
//! fit, with the variance from the curvature there. A station with few
//! outcomes is uncertain and sometimes draws high, so it gets tried; one
//! that has shown what it does draws close to its estimate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::path::PathBuf;

use super::{Activity, Domain, Labels};

/// One finished task: who worked on it and how it was judged.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub ts: DateTime<Utc>,
    pub project: String,
    #[serde(flatten)]
    pub labels: Labels,
    /// Stations that executed part of the task (the paper's S(x)).
    pub stations: Vec<String>,
    /// Judged probability that the task succeeded (`qa`).
    pub success: f64,
    /// Dollars each station spent on it.
    pub cost: BTreeMap<String, f64>,
    pub secs: u64,
    /// A segment that ran out of turns or time, learned from on the spot
    /// (a weak failure for that station alone); ground truth for the task
    /// doesn't replace it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// Ground truth for a task, appended to the same log after it was judged:
/// a benchmark's oracle score for the project the task ran in. The truth
/// scores the whole job, so it is credited by role: the project's last
/// judged outcome (the one that produced the deliverable; with the pipeline
/// usually the only one) counts with the true score, and every earlier one
/// is capped at it but never raised: an attempt the judge failed stays a
/// failure even if a later attempt got the job done, and a judge's
/// generosity is pulled down to the truth. Partial outcomes are left alone.
/// The judged values stay in the log, for calibrating the judge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Truth {
    pub ts: DateTime<Utc>,
    pub project: String,
    /// 0–1.
    pub truth: f64,
    /// Where it came from, e.g. `harness-bench:091-financial-close-reconciliation`.
    pub source: String,
}

/// Difficulty assumed when a task has none: moderate.
pub const MODERATE: f64 = 2.0;
/// Logits of ability lost per difficulty level, before a station's outcomes
/// move its own slope, and the prior spread of that slope around it.
const SLOPE: f64 = 1.2;
const SLOPE_SD: f64 = 1.0;
/// Prior spread of a station's overall ability, and of an activity or
/// domain effect around it.
const STATION_SD: f64 = 1.5;
const FACET_SD: f64 = 0.75;
/// Outcomes a station's own cost factor is shrunk by: with n of its own,
/// its factor counts n / (n + this).
const COST_PRIOR_WEIGHT: f64 = 2.0;
/// Fewest cost observations before the difficulty curve is fitted.
const COST_FIT_MIN: usize = 8;
/// Rounds of the joint fit; each refits every parameter given the others.
const SWEEPS: usize = 50;

struct Obs {
    /// Judged success.
    y: f64,
    d: f64,
    activity: Option<Activity>,
    domain: Option<Domain>,
}

#[derive(Default)]
struct Station {
    obs: Vec<Obs>,
    /// Observed dollars per task, with its difficulty.
    cost: Vec<(f64, f64)>,
    theta: f64,
    /// The station's slope's departure from SLOPE.
    slope: f64,
    activity: HashMap<Activity, f64>,
    domain: HashMap<Domain, f64>,
    /// The joint posterior's covariance at the fit, over θ, the slope, then
    /// the fitted activities and domains in `index` order.
    cov: Vec<Vec<f64>>,
    index: HashMap<Effect, usize>,
}

/// A parameter's place in the covariance matrix.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Effect {
    Activity(Activity),
    Domain(Domain),
}

impl Station {
    fn effects(&self, activity: Option<Activity>, domain: Option<Domain>) -> f64 {
        activity.and_then(|a| self.activity.get(&a)).unwrap_or(&0.0) + domain.and_then(|d| self.domain.get(&d)).unwrap_or(&0.0)
    }

    fn offset(&self, d: f64) -> f64 {
        (SLOPE + self.slope) * (d - MODERATE)
    }

    /// Coordinate ascent on the joint log-posterior (concave in each
    /// parameter): θ given the rest, the slope given the rest, then each
    /// effect given the rest.
    fn refit(&mut self) {
        for _ in 0..SWEEPS {
            let mut moved = 0.0f64;
            let offsets: Vec<f64> = self.obs.iter().map(|o| self.effects(o.activity, o.domain) - self.offset(o.d)).collect();
            let theta = fit(self.obs.iter().zip(&offsets).map(|(o, off)| (o.y, *off, 1.0)), self.theta, 0.0, STATION_SD);
            moved = moved.max((theta - self.theta).abs());
            self.theta = theta;
            // logit = (everything but the slope's part) − slope·(d − 2).
            let rest: Vec<(f64, f64, f64)> = self
                .obs
                .iter()
                .map(|o| (o.y, self.theta + self.effects(o.activity, o.domain) - SLOPE * (o.d - MODERATE), MODERATE - o.d))
                .collect();
            let slope = fit(rest.into_iter(), self.slope, 0.0, SLOPE_SD);
            moved = moved.max((slope - self.slope).abs());
            self.slope = slope;
            moved = moved.max(self.refit_facet(|o| o.activity, |s| &mut s.activity));
            moved = moved.max(self.refit_facet(|o| o.domain, |s| &mut s.domain));
            if moved < 1e-6 {
                break;
            }
        }
        // Laplace: the covariance is the inverse of the log-posterior's
        // negative Hessian, prior precision plus Σ p(1 − p)·x xᵀ, where x is
        // how much each parameter moves an outcome's logit: 1 for θ and the
        // effects it has, −(d − 2) for the slope.
        let mut index = HashMap::new();
        for o in &self.obs {
            for k in [o.activity.map(Effect::Activity), o.domain.map(Effect::Domain)].into_iter().flatten() {
                let n = index.len() + 2;
                index.entry(k).or_insert(n);
            }
        }
        let n = index.len() + 2;
        let mut h = vec![vec![0.0; n]; n];
        for (i, row) in h.iter_mut().enumerate() {
            row[i] = 1.0 / [STATION_SD, SLOPE_SD].get(i).copied().unwrap_or(FACET_SD).powi(2);
        }
        for o in &self.obs {
            let p = sigmoid(self.theta + self.effects(o.activity, o.domain) - self.offset(o.d));
            let w = p * (1.0 - p);
            let touched: Vec<(usize, f64)> = [(0, 1.0), (1, MODERATE - o.d)]
                .into_iter()
                .chain([o.activity.map(Effect::Activity), o.domain.map(Effect::Domain)].into_iter().flatten().map(|k| (index[&k], 1.0)))
                .collect();
            for &(a, xa) in &touched {
                for &(b, xb) in &touched {
                    h[a][b] += w * xa * xb;
                }
            }
        }
        self.cov = invert(h);
        self.index = index;
    }

    /// Refit every effect of one facet given θ and the other facet.
    fn refit_facet<K: Copy + Eq + Hash>(&mut self, key: fn(&Obs) -> Option<K>, facet: fn(&mut Station) -> &mut HashMap<K, f64>) -> f64 {
        let keys: Vec<K> = {
            let mut ks: Vec<K> = Vec::new();
            for k in self.obs.iter().filter_map(key) {
                if !ks.contains(&k) {
                    ks.push(k);
                }
            }
            ks
        };
        let mut moved = 0.0f64;
        for k in keys {
            let own = facet(self).get(&k).copied().unwrap_or(0.0);
            // Everything but this effect, per observation that has it.
            let obs: Vec<(f64, f64, f64)> = self
                .obs
                .iter()
                .filter(|o| key(o) == Some(k))
                .map(|o| (o.y, self.theta + self.effects(o.activity, o.domain) - own - self.offset(o.d), 1.0))
                .collect();
            let new = fit(obs.into_iter(), own, 0.0, FACET_SD);
            moved = moved.max((new - own).abs());
            facet(self).insert(k, new);
        }
        moved
    }
}

/// A learned activity or domain effect: its name, logits, outcomes behind it.
pub type Facet = (&'static str, f64, usize);

/// A small seeded generator for routing draws (SplitMix64; normals by
/// Box-Muller). Not for anything that needs real randomness.
pub struct Rng(u64);

impl Rng {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// Seeded from the clock and the process id.
    pub fn from_time() -> Rng {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        Rng(nanos ^ ((std::process::id() as u64) << 32))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1).
    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal.
    pub fn normal(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (std::f64::consts::TAU * self.uniform()).cos()
    }
}

pub struct Expertise {
    path: Option<PathBuf>,
    stations: HashMap<String, Station>,
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Inverse of a symmetric positive-definite matrix (Gauss-Jordan; the
/// matrices here are at most 30 × 30).
fn invert(mut a: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    let n = a.len();
    let mut inv: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect()).collect();
    for col in 0..n {
        let pivot = (col..n).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs())).unwrap_or(col);
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let d = a[col][col];
        for j in 0..n {
            a[col][j] /= d;
            inv[col][j] /= d;
        }
        for row in 0..n {
            if row != col {
                let f = a[row][col];
                if f != 0.0 {
                    for j in 0..n {
                        a[row][j] -= f * a[col][j];
                        inv[row][j] -= f * inv[col][j];
                    }
                }
            }
        }
    }
    inv
}

/// MAP value of one parameter x for observations (y, o, w) with
/// P(y) = σ(o + w·x), under a normal prior (Newton's method; the
/// log-posterior is concave in x).
fn fit(obs: impl Iterator<Item = (f64, f64, f64)>, start: f64, prior_mean: f64, prior_sd: f64) -> f64 {
    let obs: Vec<(f64, f64, f64)> = obs.collect();
    let mut x = start;
    for _ in 0..25 {
        let (mut grad, mut curv) = (-(x - prior_mean) / prior_sd.powi(2), -1.0 / prior_sd.powi(2));
        for (y, o, w) in &obs {
            let p = sigmoid(o + w * x);
            grad += w * (y - p);
            curv -= w * w * p * (1.0 - p);
        }
        let step = grad / curv;
        x -= step;
        if step.abs() < 1e-9 {
            break;
        }
    }
    x
}

impl Expertise {
    /// Fold the outcome log at `path`; a missing or partly corrupt log just
    /// contributes what parses.
    pub fn load(path: Option<PathBuf>) -> Expertise {
        let mut me = Expertise { path: None, stations: HashMap::new() };
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            // Truths first (the latest per project wins), then the outcomes
            // they correct.
            let mut truths: HashMap<String, f64> = HashMap::new();
            for line in text.lines() {
                if let Ok(t) = serde_json::from_str::<Truth>(line) {
                    truths.insert(t.project.trim_end_matches('/').to_string(), t.truth.clamp(0.0, 1.0));
                }
            }
            let outcomes: Vec<Outcome> = text.lines().filter_map(|l| serde_json::from_str::<Outcome>(l).ok()).collect();
            // The last judged outcome per project with a truth.
            let mut last: HashMap<&str, usize> = HashMap::new();
            for (i, o) in outcomes.iter().enumerate() {
                if !o.partial && truths.contains_key(o.project.trim_end_matches('/')) {
                    last.insert(o.project.trim_end_matches('/'), i);
                }
            }
            for (i, mut outcome) in outcomes.iter().cloned().enumerate() {
                let project = outcome.project.trim_end_matches('/').to_string();
                if !outcome.partial
                    && let Some(&t) = truths.get(&project)
                {
                    outcome.success = if last.get(project.as_str()) == Some(&i) { t } else { outcome.success.min(t) };
                }
                me.fold(&outcome);
            }
        }
        for s in me.stations.values_mut() {
            s.refit();
        }
        me.path = path;
        me
    }

    pub fn default_path() -> Option<PathBuf> {
        let dir = crate::config::Config::home_dir().ok()?.join("deals");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join("outcomes.jsonl"))
    }

    fn fold(&mut self, o: &Outcome) {
        let y = o.success.clamp(0.0, 1.0);
        let d = o.labels.difficulty.unwrap_or(MODERATE).clamp(0.0, 4.0);
        for station in &o.stations {
            let s = self.stations.entry(station.clone()).or_default();
            s.obs.push(Obs { y, d, activity: o.labels.activity, domain: o.labels.domain });
        }
        for (station, cost) in &o.cost {
            if *cost > 0.0 {
                self.stations.entry(station.clone()).or_default().cost.push((d, *cost));
            }
        }
    }

    /// Learn from an outcome and append it to the log.
    pub fn record(&mut self, outcome: Outcome) {
        self.fold(&outcome);
        for name in outcome.stations.iter().chain(outcome.cost.keys()).collect::<std::collections::BTreeSet<_>>() {
            if let Some(s) = self.stations.get_mut(name) {
                s.refit();
            }
        }
        let Some(path) = &self.path else { return };
        let line = match serde_json::to_string(&outcome) {
            Ok(l) => l,
            Err(e) => return tracing::warn!("deals: outcome not serializable: {e}"),
        };
        use std::io::Write;
        let written = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = written {
            tracing::warn!("deals: outcome log write failed (continuing): {e}");
        }
    }

    /// Outcomes this station has been judged on, all labels together.
    pub fn outcomes(&self, model: &str) -> usize {
        self.stations.get(model).map_or(0, |s| s.obs.len())
    }

    /// Predicted chance that this station succeeds at a task with these labels.
    pub fn p(&self, model: &str, labels: &Labels) -> f64 {
        let d = labels.difficulty.unwrap_or(MODERATE);
        let logit = self.stations.get(model).map_or(-SLOPE * (d - MODERATE), |s| s.theta + s.effects(labels.activity, labels.domain) - s.offset(d));
        sigmoid(logit)
    }

    /// A draw of this station's chance at a task with these labels, from
    /// the posterior: what routing uses when it explores. An unknown station
    /// draws from the priors alone.
    pub fn sample(&self, model: &str, labels: &Labels, rng: &mut Rng) -> f64 {
        let d = labels.difficulty.unwrap_or(MODERATE);
        let facet = FACET_SD.powi(2);
        let wanted = [labels.activity.map(Effect::Activity), labels.domain.map(Effect::Domain)];
        let x = MODERATE - d;
        let (mean, var) = match self.stations.get(model) {
            None => (SLOPE * x, STATION_SD.powi(2) + (SLOPE_SD * x).powi(2) + facet * wanted.iter().flatten().count() as f64),
            Some(s) => {
                // Var(θ + α + β − slope·(d − 2)) = cᵀ Σ c over the fitted
                // ones; a facet the station has no outcomes in adds its prior.
                let mut c = vec![(0usize, 1.0), (1, x)];
                let mut unseen = 0.0;
                for k in wanted.into_iter().flatten() {
                    match s.index.get(&k) {
                        Some(&i) => c.push((i, 1.0)),
                        None => unseen += facet,
                    }
                }
                let var: f64 = c.iter().map(|&(a, ca)| c.iter().map(|&(b, cb)| ca * cb * s.cov[a][b]).sum::<f64>()).sum::<f64>() + unseen;
                (s.theta + s.effects(labels.activity, labels.domain) - s.offset(d), var.max(0.0))
            }
        };
        sigmoid(mean + var.sqrt() * rng.normal())
    }

    /// Overall ability, then each fitted effect with its outcome count —
    /// strongest first (`tursi --stations`).
    pub fn facets(&self, model: &str) -> Option<(f64, Vec<Facet>)> {
        let s = self.stations.get(model)?;
        let mut out: Vec<Facet> = s
            .activity
            .iter()
            .map(|(a, x)| (a.name(), *x, s.obs.iter().filter(|o| o.activity == Some(*a)).count()))
            .chain(s.domain.iter().map(|(d, x)| (d.name(), *x, s.obs.iter().filter(|o| o.domain == Some(*d)).count())))
            .collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        Some((s.theta, out))
    }

    /// The cost model routing uses (`CostModel`): fitted over every
    /// station's observed costs, `guess` giving each station's price-based
    /// guess.
    pub fn cost_model(&self, guess: impl Fn(&str) -> Option<f64>) -> CostModel {
        // Pooled least squares of log(cost / guess) on difficulty.
        let obs: Vec<(&str, f64, f64)> = self
            .stations
            .iter()
            .filter_map(|(m, s)| guess(m).filter(|g| *g > 0.0).map(|g| (m, s, g)))
            .flat_map(|(m, s, g)| s.cost.iter().map(move |(d, c)| (m.as_str(), *d, (c / g).ln())))
            .collect();
        if obs.len() < COST_FIT_MIN {
            return CostModel::default();
        }
        let n = obs.len() as f64;
        let (mx, my) = (obs.iter().map(|o| o.1).sum::<f64>() / n, obs.iter().map(|o| o.2).sum::<f64>() / n);
        let sxx: f64 = obs.iter().map(|o| (o.1 - mx).powi(2)).sum();
        let slope = if sxx > 1e-9 { obs.iter().map(|o| (o.1 - mx) * (o.2 - my)).sum::<f64>() / sxx } else { 0.0 };
        // More tokens for harder work, never fewer; at most ~4.5x a level.
        let slope = slope.clamp(0.0, 1.5);
        let level = my - slope * mx;
        // Each station's own deviation from the curve, shrunk toward none.
        let mut dev: HashMap<String, (f64, f64)> = HashMap::new();
        for (m, d, y) in &obs {
            let e = dev.entry(m.to_string()).or_default();
            e.0 += y - (level + slope * d);
            e.1 += 1.0;
        }
        let factor = dev.into_iter().map(|(m, (sum, k))| (m, sum / (k + COST_PRIOR_WEIGHT))).collect();
        CostModel { level, slope, factor }
    }
}

/// Expected dollars for a task: the station's price-based guess, times a
/// curve in difficulty fitted across all stations (harder work reads and
/// writes more tokens: on the first 230 outcomes real cost ran from about
/// 0.1x the guess for trivial tasks to 0.7x for very hard ones), times the
/// station's own learned factor (a reasoning-heavy model can run several
/// times the curve). An untried station has factor 1; with no data the
/// model is the guess itself.
#[derive(Default)]
pub struct CostModel {
    level: f64,
    slope: f64,
    factor: HashMap<String, f64>,
}

impl CostModel {
    pub fn expect(&self, model: &str, guess: f64, difficulty: Option<f64>) -> f64 {
        let d = difficulty.unwrap_or(MODERATE);
        guess * (self.level + self.slope * d + self.factor.get(model).copied().unwrap_or(0.0)).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(activity: Activity, domain: Domain, d: f64) -> Labels {
        Labels { activity: Some(activity), domain: Some(domain), difficulty: Some(d) }
    }

    fn outcome(stations: &[&str], l: Labels, success: f64) -> Outcome {
        Outcome {
            ts: Utc::now(),
            project: "p".into(),
            labels: l,
            stations: stations.iter().map(|s| s.to_string()).collect(),
            success,
            cost: stations.iter().map(|s| (s.to_string(), 0.01)).collect(),
            secs: 10,
            partial: false,
        }
    }

    use Activity::*;
    use Domain::*;

    #[test]
    fn an_unknown_station_is_even_at_moderate_and_harder_tasks_are_less_likely() {
        let e = Expertise::load(None);
        assert!((e.p("m", &labels(New, Backend, MODERATE)) - 0.5).abs() < 1e-9);
        assert!((e.p("m", &Labels::default()) - 0.5).abs() < 1e-9, "unlabelled is moderate");
        assert!(e.p("m", &labels(New, Backend, 0.0)) > 0.9 && e.p("m", &labels(New, Backend, 4.0)) < 0.1);
    }

    #[test]
    fn a_small_model_learns_its_limit_from_easy_wins_and_hard_losses() {
        let mut e = Expertise::load(None);
        for _ in 0..6 {
            e.record(outcome(&["small"], labels(New, Backend, 1.0), 1.0));
            e.record(outcome(&["small"], labels(New, Backend, 3.0), 0.0));
            e.record(outcome(&["big"], labels(New, Backend, 3.0), 1.0));
        }
        let (easy, hard) = (e.p("small", &labels(New, Backend, 1.0)), e.p("small", &labels(New, Backend, 3.0)));
        assert!(easy > 0.7 && hard < 0.3, "easy {easy:.2} hard {hard:.2}");
        assert!(e.p("big", &labels(New, Backend, 3.0)) > 0.8);
    }

    #[test]
    fn a_strong_station_starts_strong_at_work_it_has_not_tried() {
        let mut e = Expertise::load(None);
        for _ in 0..8 {
            e.record(outcome(&["strong"], labels(New, Backend, 2.0), 1.0));
            e.record(outcome(&["weak"], labels(New, Backend, 2.0), 0.0));
        }
        let unseen = labels(Writing, Prose, 2.0);
        assert!(e.p("strong", &unseen) > 0.7);
        assert!(e.p("weak", &unseen) < 0.3);
        // A failure at the new work still counts for more than the prior.
        let before = e.p("strong", &unseen);
        e.record(outcome(&["strong"], unseen, 0.0));
        assert!(e.p("strong", &unseen) < before);
    }

    #[test]
    fn a_weak_domain_is_learned_and_carries_to_other_activities_in_it() {
        let mut e = Expertise::load(None);
        for _ in 0..8 {
            e.record(outcome(&["m"], labels(New, Backend, 2.0), 1.0));
            e.record(outcome(&["m"], labels(Debug, Backend, 2.0), 1.0));
            e.record(outcome(&["m"], labels(New, Systems, 2.0), 0.0));
        }
        // Never debugged systems code: worse than backend, from the domain alone.
        let (systems, backend) = (e.p("m", &labels(Debug, Systems, 2.0)), e.p("m", &labels(Debug, Backend, 2.0)));
        assert!(systems < backend - 0.2, "systems {systems:.2} backend {backend:.2}");
        let (theta, facets) = e.facets("m").unwrap();
        assert!(theta > 0.0);
        assert_eq!(facets.last().unwrap().0, "systems", "{facets:?}");
    }

    #[test]
    fn draws_spread_for_the_unknown_and_tighten_with_outcomes() {
        let mut e = Expertise::load(None);
        for _ in 0..30 {
            e.record(outcome(&["known"], labels(New, Backend, 2.0), 1.0));
            e.record(outcome(&["known"], labels(New, Backend, 2.0), 0.0));
        }
        let task = labels(New, Backend, 2.0);
        let mut rng = Rng::new(7);
        let spread = |e: &Expertise, m: &str, rng: &mut Rng| {
            let draws: Vec<f64> = (0..2000).map(|_| e.sample(m, &task, rng)).collect();
            let mean = draws.iter().sum::<f64>() / draws.len() as f64;
            let sd = (draws.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / draws.len() as f64).sqrt();
            (mean, sd, draws.iter().filter(|x| **x > 0.8).count())
        };
        let (known_mean, known_sd, known_high) = spread(&e, "known", &mut rng);
        let (new_mean, new_sd, new_high) = spread(&e, "never-tried", &mut rng);
        assert!((known_mean - 0.5).abs() < 0.05 && known_sd < 0.1, "known {known_mean:.2} ± {known_sd:.2}");
        assert!((new_mean - 0.5).abs() < 0.05 && new_sd > 0.25, "unknown {new_mean:.2} ± {new_sd:.2}");
        assert!(known_high < 20 && new_high > 300, "an unknown station sometimes draws high: {known_high} vs {new_high}");
    }

    #[test]
    fn cost_grows_with_difficulty_and_a_dear_station_learns_its_factor() {
        let mut e = Expertise::load(None);
        let guess = |m: &str| Some(if m == "pricey" { 0.10 } else { 0.01 });
        assert_eq!(e.cost_model(guess).expect("cheap", 0.01, Some(3.0)), 0.01, "no data: the guess");
        // Cheap runs at the curve: 0.002 easy, 0.008 hard (2x a level).
        for _ in 0..10 {
            for (d, c) in [(1.0, 0.002), (2.0, 0.004), (3.0, 0.008)] {
                let mut o = outcome(&["cheap"], labels(New, Backend, d), 1.0);
                o.cost = BTreeMap::from([("cheap".to_string(), c)]);
                e.record(o);
            }
        }
        let m = e.cost_model(guess);
        let (easy, hard) = (m.expect("cheap", 0.01, Some(1.0)), m.expect("cheap", 0.01, Some(3.0)));
        assert!((hard / easy - 4.0).abs() < 0.3, "two levels harder, four times dearer: {easy:.4} {hard:.4}");
        // An untried station follows the same curve from its own guess.
        assert!((m.expect("untried", 0.10, Some(3.0)) / m.expect("untried", 0.10, Some(1.0)) - 4.0).abs() < 0.3);
        // Pricey runs 5x its curve; after a few outcomes its factor shows it.
        for _ in 0..6 {
            let mut o = outcome(&["pricey"], labels(New, Backend, 3.0), 1.0);
            o.cost = BTreeMap::from([("pricey".to_string(), 0.10 * 0.8 * 5.0)]);
            e.record(o);
        }
        let m = e.cost_model(guess);
        let curve = m.expect("untried", 0.10, Some(3.0));
        let learned = m.expect("pricey", 0.10, Some(3.0));
        // Its real cost is 0.40; the curve alone (an untried station) says far less.
        assert!(learned > 2.0 * curve && (learned - 0.40).abs() < 0.12, "pricey learned its cost: {learned:.3} (curve {curve:.3})");
    }

    #[test]
    fn ground_truth_appended_later_replaces_the_judge() {
        let dir = std::env::temp_dir().join(format!("tursi-deals-truth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("outcomes.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut e = Expertise::load(Some(path.clone()));
        // The judge was generous: 0.95 for work the bench scored 0.2.
        for project in ["bench/a", "bench/b"] {
            let mut o = outcome(&["m"], labels(Analyze, Data, 2.0), 0.95);
            o.project = project.into();
            e.record(o);
        }
        let mut cut = outcome(&["m"], labels(Analyze, Data, 2.0), 0.2);
        cut.project = "bench/a".into();
        cut.partial = true;
        e.record(cut);
        let judged = Expertise::load(Some(path.clone())).p("m", &labels(Analyze, Data, 2.0));
        let truth = Truth { ts: Utc::now(), project: "bench/a/".into(), truth: 0.2, source: "test".into() };
        let mut log = std::fs::read_to_string(&path).unwrap();
        log.push_str(&serde_json::to_string(&truth).unwrap());
        log.push('\n');
        std::fs::write(&path, &log).unwrap();
        let corrected = Expertise::load(Some(path.clone()));
        assert!(corrected.p("m", &labels(Analyze, Data, 2.0)) < judged - 0.1, "the truth pulls it down");
        assert_eq!(corrected.outcomes("m"), 3, "corrected, not added");
        // A job where a weak model failed first and a strong one then did it:
        // the failure isn't credited with the job's success.
        let mut weak = outcome(&["weak"], labels(Analyze, Data, 2.0), 0.05);
        weak.project = "bench/c".into();
        let mut strong = outcome(&["strong"], labels(Analyze, Data, 2.0), 0.4);
        strong.project = "bench/c".into();
        let mut log = std::fs::read_to_string(&path).unwrap();
        for o in [&weak, &strong] {
            log.push_str(&serde_json::to_string(o).unwrap());
            log.push('\n');
        }
        log.push_str(&serde_json::to_string(&Truth { ts: Utc::now(), project: "bench/c".into(), truth: 1.0, source: "test".into() }).unwrap());
        log.push('\n');
        std::fs::write(&path, &log).unwrap();
        let e = Expertise::load(Some(path.clone()));
        let l = labels(Analyze, Data, 2.0);
        assert!(e.p("weak", &l) < 0.4 && e.p("strong", &l) > 0.6, "weak {:.2} strong {:.2}", e.p("weak", &l), e.p("strong", &l));
        // The judged value stays in the log for calibrating the judge later.
        assert!(log.contains(r#""success":0.95"#));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_log_is_the_state() {
        let dir = std::env::temp_dir().join(format!("tursi-deals-exp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("outcomes.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut e = Expertise::load(Some(path.clone()));
        e.record(outcome(&["m"], labels(Review, Legal, 1.0), 1.0));
        e.record(outcome(&["m"], labels(Review, Legal, 3.5), 0.0));
        // An unlabelled line still folds (station ability only, moderate).
        let bare = r#"{"ts":"2026-10-05T00:00:00Z","project":"p","stations":["m"],"success":1.0,"cost":{},"secs":1}"#;
        std::fs::write(&path, format!("{}{bare}\nnot json\n", std::fs::read_to_string(&path).unwrap())).unwrap();
        e.record(outcome(&["m"], labels(Review, Legal, MODERATE), 1.0));
        let line = std::fs::read_to_string(&path).unwrap().lines().last().unwrap().to_string();
        assert!(line.contains(r#""activity":"review","domain":"legal","difficulty":2.0"#), "{line}");
        let again = Expertise::load(Some(path));
        assert_eq!(again.outcomes("m"), 4, "two, the bare line, and one more");
        assert!(again.p("m", &labels(Review, Legal, 1.0)) > again.p("m", &labels(Review, Legal, 3.5)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
