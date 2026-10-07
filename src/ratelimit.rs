//! Request-rate limits (§7.2): one token bucket per model and one per
//! provider gateway, shared by every agent in the process. The provider's
//! limits are per account, so they cannot live in one agent loop: this is the
//! one deliberate exception to §5.7's no-global-state rule. A 429 empties the
//! model's bucket and stalls it briefly; DEALS stations read the stall to
//! shrink their slots (§5.8).

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::LimitsConfig;

/// How long a 429 stalls a model before requests resume.
const STALL: Duration = Duration::from_secs(20);
/// A model throttled this recently counts as saturated.
const RECENT: Duration = Duration::from_secs(90);

struct Bucket {
    per_sec: f64,
    cap: f64,
    tokens: f64,
    last: Instant,
    stalled_until: Option<Instant>,
    throttled_at: Option<Instant>,
}

impl Bucket {
    /// Bursts of up to ten seconds' worth, so a wave of agents starting at
    /// once spreads over the minute instead of spending it in one go.
    fn new(rpm: u32, now: Instant) -> Bucket {
        let per_sec = rpm.max(1) as f64 / 60.0;
        let cap = (per_sec * 10.0).max(1.0);
        Bucket { per_sec, cap, tokens: cap, last: now, stalled_until: None, throttled_at: None }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.per_sec).min(self.cap);
        self.last = now;
    }

    /// Time until a request may go; zero means now.
    fn wait(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if let Some(until) = self.stalled_until {
            if until > now {
                return until - now;
            }
            self.stalled_until = None;
        }
        if self.tokens >= 1.0 { Duration::ZERO } else { Duration::from_secs_f64((1.0 - self.tokens) / self.per_sec) }
    }

    fn throttle(&mut self, now: Instant) {
        self.tokens = 0.0;
        self.stalled_until = Some(now + STALL);
        self.throttled_at = Some(now);
    }
}

struct Rates {
    per_model: HashMap<String, u32>,
    default_rpm: u32,
    paid_rpm: u32,
    gateway_rpm: u32,
    paid: HashSet<String>,
}

impl Rates {
    fn model_rpm(&self, model: &str) -> u32 {
        self.per_model.get(model).copied().unwrap_or(if self.paid.contains(model) { self.paid_rpm } else { self.default_rpm })
    }
}

struct Limits {
    rates: Mutex<Rates>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

static LIMITS: OnceLock<Limits> = OnceLock::new();

fn limits() -> &'static Limits {
    LIMITS.get_or_init(|| {
        let d = LimitsConfig::default();
        Limits {
            rates: Mutex::new(Rates { per_model: d.rpm, default_rpm: d.default_rpm, paid_rpm: d.paid_rpm, gateway_rpm: d.gateway_rpm, paid: HashSet::new() }),
            buckets: Mutex::new(HashMap::new()),
        }
    })
}

/// Load the configured rates and which models are on the provider's paid
/// tier (from the station catalog). Buckets already in use keep running.
pub fn configure(config: &LimitsConfig, paid: impl IntoIterator<Item = String>) {
    let mut rates = limits().rates.lock().unwrap();
    *rates = Rates {
        per_model: config.rpm.clone(),
        default_rpm: config.default_rpm,
        paid_rpm: config.paid_rpm,
        gateway_rpm: config.gateway_rpm,
        paid: paid.into_iter().collect(),
    };
}

/// The provider prefix (`cloudflare/@cf/…` → `cloudflare`): one gateway.
fn gateway_key(model: &str) -> String {
    format!("gateway:{}", model.split('/').next().unwrap_or(model))
}

/// Requests per minute this model may send.
pub fn model_rpm(model: &str) -> u32 {
    limits().rates.lock().unwrap().model_rpm(model)
}

/// Wait until both the model's and its gateway's bucket allow a request,
/// then spend one token from each.
pub async fn acquire(model: &str) {
    loop {
        let wait = {
            let (model_rpm, gateway_rpm) = {
                let rates = limits().rates.lock().unwrap();
                (rates.model_rpm(model), rates.gateway_rpm)
            };
            let now = Instant::now();
            let mut buckets = limits().buckets.lock().unwrap();
            let m = buckets.entry(format!("model:{model}")).or_insert_with(|| Bucket::new(model_rpm, now)).wait(now);
            let g = if gateway_rpm > 0 {
                buckets.entry(gateway_key(model)).or_insert_with(|| Bucket::new(gateway_rpm, now)).wait(now)
            } else {
                Duration::ZERO
            };
            if m.is_zero() && g.is_zero() {
                if let Some(b) = buckets.get_mut(&format!("model:{model}")) {
                    b.tokens -= 1.0;
                }
                if gateway_rpm > 0 {
                    if let Some(b) = buckets.get_mut(&gateway_key(model)) {
                        b.tokens -= 1.0;
                    }
                }
                return;
            }
            m.max(g)
        };
        tokio::time::sleep(wait).await;
    }
}

/// The provider answered 429: stop sending to this model for a while.
pub fn throttled(model: &str) {
    let rpm = model_rpm(model);
    let now = Instant::now();
    let mut buckets = limits().buckets.lock().unwrap();
    buckets.entry(format!("model:{model}")).or_insert_with(|| Bucket::new(rpm, now)).throttle(now);
    tracing::warn!(%model, "rate limited — stalling this model for {}s", STALL.as_secs());
}

/// Whether the model hit a 429 recently (stations shrink their slots).
pub fn recently_throttled(model: &str) -> bool {
    let buckets = limits().buckets.lock().unwrap();
    buckets.get(&format!("model:{model}")).and_then(|b| b.throttled_at).is_some_and(|t| t.elapsed() < RECENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_bursts_ten_seconds_then_paces() {
        let t0 = Instant::now();
        let mut b = Bucket::new(60, t0); // one per second, burst of ten
        for _ in 0..10 {
            assert!(b.wait(t0).is_zero());
            b.tokens -= 1.0;
        }
        let w = b.wait(t0);
        assert!(w > Duration::from_millis(900) && w <= Duration::from_secs(1), "{w:?}");
        assert!(b.wait(t0 + Duration::from_secs(1)).is_zero());
    }

    #[test]
    fn a_429_stalls_the_bucket() {
        let t0 = Instant::now();
        let mut b = Bucket::new(300, t0);
        b.throttle(t0);
        assert!(b.wait(t0 + Duration::from_secs(5)) >= Duration::from_secs(14));
        assert!(b.wait(t0 + STALL + Duration::from_secs(1)).is_zero(), "refilled after the stall");
    }

    #[test]
    fn paid_models_get_the_paid_rate() {
        let rates = Rates {
            per_model: HashMap::from([("x/pinned".to_string(), 7)]),
            default_rpm: 300,
            paid_rpm: 50,
            gateway_rpm: 200,
            paid: HashSet::from(["x/paid".to_string()]),
        };
        assert_eq!(rates.model_rpm("x/paid"), 50);
        assert_eq!(rates.model_rpm("x/free"), 300);
        assert_eq!(rates.model_rpm("x/pinned"), 7);
        assert_eq!(gateway_key("cloudflare/@cf/zai-org/glm-5.3"), "gateway:cloudflare");
    }
}
