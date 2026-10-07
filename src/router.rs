//! Model selection: one configured model, API-failure fallbacks, and the
//! `/model` pin. No escalation — the model never changes on task signals.

pub struct Router {
    /// The configured model first, then its fallbacks in order.
    models: Vec<String>,
    /// Index into `models`, advanced by provider failover.
    model: usize,
    pinned: Option<String>,
}

impl Router {
    pub fn new(model: String, fallbacks: Vec<String>) -> Router {
        let mut models = vec![model];
        models.extend(fallbacks);
        Router { models, model: 0, pinned: None }
    }

    /// Active model id (`/model` pin overrides the configuration).
    pub fn model(&self) -> &str {
        match &self.pinned {
            Some(pinned) => pinned,
            None => &self.models[self.model],
        }
    }

    /// Provider/API failure: next fallback, or None when exhausted. A pin
    /// means the user chose — never fail over away from it.
    pub fn failover(&mut self) -> Option<String> {
        if self.pinned.is_some() || self.model + 1 >= self.models.len() {
            return None;
        }
        self.model += 1;
        Some(self.model().to_string())
    }

    /// New task: back to the configured model. A pin survives until cleared.
    pub fn reset(&mut self) {
        self.model = 0;
    }

    pub fn pin(&mut self, model: Option<String>) {
        self.pinned = model;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_walks_the_fallbacks_and_reset_returns_to_the_model() {
        let mut r = Router::new("a/one".into(), vec!["a/two".into()]);
        assert_eq!(r.model(), "a/one");
        assert_eq!(r.failover().as_deref(), Some("a/two"));
        assert_eq!(r.failover(), None);
        r.reset();
        assert_eq!(r.model(), "a/one");
    }

    #[test]
    fn a_pin_overrides_the_model_and_blocks_failover() {
        let mut r = Router::new("a/one".into(), vec!["a/two".into()]);
        r.pin(Some("x/pinned".into()));
        assert_eq!(r.model(), "x/pinned");
        assert_eq!(r.failover(), None);
        r.pin(None);
        assert_eq!(r.model(), "a/one");
    }
}
