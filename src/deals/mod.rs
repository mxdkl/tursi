//! DEALS task serving (§5.8, arXiv 2609.33768): subagent tasks queue at
//! model stations, and each station routes by backlog, learned success rate
//! for the task's labels, and cost — no model call decides where a task runs.
//!
//! Every task is labelled at intake (`labels`: activity, domain,
//! difficulty). A station is one model with a queue per activity, a fixed
//! number of execution slots, and a private memory of successful
//! trajectories. When a
//! slot frees, the station takes the head of its longest queue and either
//! forwards it to a better-placed station or runs it. A run ends with an
//! answer or a split: the finished part goes back to the queue as a
//! continuation another station may resume. Finished tasks are judged (`qa`),
//! and the verdict updates the success estimates of every station that
//! worked on them (`expertise`).

pub mod catalog;
pub mod expertise;
pub mod labels;
pub mod memory;
pub mod pool;
pub mod qa;
pub mod router;

pub use labels::{Activity, Domain, Labels};

/// `cloudflare/@cf/zai-org/glm-5.3-flash` → `glm-5.3-flash`: for reports,
/// the UI, and file names.
pub fn short_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(short_model("cloudflare/@cf/zai-org/glm-5.3-flash"), "glm-5.3-flash");
    }
}
