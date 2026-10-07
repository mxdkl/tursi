//! The DEALS routing rule (§5.8), quality first. Of the stations that could
//! take a task (the one holding it and its eligible neighbors), keep those
//! whose chance of success q is within `tolerance` of the best one's (the
//! station it just came from counts toward the best), and send the task to
//! the cheapest of them. A cheaper station never wins by being
//! less likely to get the task done: cost only decides between stations about
//! as likely to succeed as the best. Between stations that also cost about
//! the same, the shorter backlog B (queued + in flight) for the task's type
//! wins, with the holder credited one task so equals don't trade work back
//! and forth. Pure functions: the pool gathers the advertisements.

/// What a station advertises for one task type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ad {
    pub backlog: f64,
    pub q: f64,
    pub cost: f64,
}

/// Expected costs within this ratio of the cheapest count as the same price.
const SAME_PRICE: f64 = 1.1;

/// The neighbor to forward to, or None to run here. `neighbors` are the
/// eligible ones (not the previous station, hop limit not reached, able to
/// run the type). `rivals` count toward the best chance but can't take the
/// task: the station it just came from, so a hop away from the best doesn't
/// lower the bar for the next one. With `stay` false the task must leave (it
/// ran out of time here), so the holder is not a candidate. Remaining ties go
/// to the lowest index, as in the paper.
pub fn choose(here: &Ad, neighbors: &[(usize, Ad)], rivals: &[Ad], tolerance: f64, stay: bool) -> Option<usize> {
    let mut candidates: Vec<(Option<usize>, Ad)> = neighbors.iter().map(|(j, ad)| (Some(*j), *ad)).collect();
    if stay {
        candidates.push((None, *here));
    }
    let best = candidates.iter().map(|(_, ad)| ad.q).chain(rivals.iter().map(|ad| ad.q)).fold(f64::NEG_INFINITY, f64::max);
    candidates.retain(|(_, ad)| ad.q >= best - tolerance);
    let cheapest = candidates.iter().map(|(_, ad)| ad.cost).fold(f64::INFINITY, f64::min);
    candidates.retain(|(_, ad)| ad.cost <= cheapest * SAME_PRICE);
    let load = |(j, ad): &(Option<usize>, Ad)| if j.is_none() { ad.backlog - 1.0 } else { ad.backlog };
    candidates
        .into_iter()
        .min_by(|a, b| load(a).total_cmp(&load(b)).then_with(|| a.0.map_or(-1, |j| j as i64).cmp(&b.0.map_or(-1, |j| j as i64))))
        .and_then(|(j, _)| j)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad(backlog: f64, q: f64, cost: f64) -> Ad {
        Ad { backlog, q, cost }
    }

    #[test]
    fn the_most_likely_to_succeed_wins_whatever_it_costs() {
        let here = ad(0.0, 0.70, 0.01);
        let strong = ad(5.0, 0.85, 0.20);
        assert_eq!(choose(&here, &[(3, strong)], &[], 0.03, true), Some(3), "20x the price and a queue, but more likely to get it done");
        assert_eq!(choose(&strong, &[(3, here)], &[], 0.03, true), None, "and it keeps work it holds");
    }

    #[test]
    fn among_stations_about_as_good_as_the_best_the_cheapest_wins() {
        let here = ad(0.0, 0.97, 0.05);
        let cheap = ad(2.0, 0.95, 0.005);
        let cheaper_but_worse = ad(0.0, 0.90, 0.001);
        assert_eq!(choose(&here, &[(1, cheap), (2, cheaper_but_worse)], &[], 0.03, true), Some(1));
        // A wider tolerance lets the worse one in.
        assert_eq!(choose(&here, &[(1, cheap), (2, cheaper_but_worse)], &[], 0.1, true), Some(2));
    }

    #[test]
    fn backlog_decides_between_equals_and_the_holder_gets_one_task_of_credit() {
        let here = ad(5.0, 0.8, 0.01);
        assert_eq!(choose(&here, &[(1, ad(1.0, 0.8, 0.01)), (2, ad(3.0, 0.8, 0.0105))], &[], 0.03, true), Some(1));
        // A one-task difference is not worth a hop.
        assert_eq!(choose(&ad(2.0, 0.8, 0.01), &[(1, ad(1.0, 0.8, 0.01))], &[], 0.03, true), None);
        // A clearly cheaper equal takes it despite the longer queue.
        assert_eq!(choose(&ad(0.0, 0.8, 0.01), &[(1, ad(6.0, 0.8, 0.002))], &[], 0.03, true), Some(1));
    }

    #[test]
    fn a_task_that_must_leave_goes_to_the_best_neighbor() {
        let here = ad(0.0, 0.99, 0.001);
        assert_eq!(choose(&here, &[(1, ad(0.0, 0.5, 0.01)), (2, ad(0.0, 0.7, 0.05))], &[], 0.03, false), Some(2));
        assert_eq!(choose(&here, &[], &[], 0.03, false), None, "nowhere else: it stays");
    }

    #[test]
    fn the_station_a_task_came_from_still_sets_the_bar() {
        // Forwarded from a 0.90 station to this 0.88 one: a 0.86 neighbor is
        // cheaper, but 0.04 short of the best, so the task stays.
        let here = ad(0.0, 0.88, 0.02);
        let cheaper = (1, ad(0.0, 0.86, 0.005));
        assert_eq!(choose(&here, &[cheaper], &[ad(0.0, 0.90, 0.1)], 0.03, true), None);
        assert_eq!(choose(&here, &[cheaper], &[], 0.03, true), Some(1), "without it the bar would drop");
    }

    #[test]
    fn ties_go_to_the_lowest_index() {
        let here = ad(6.0, 0.5, 0.01);
        let n = ad(0.0, 0.5, 0.01);
        assert_eq!(choose(&here, &[(4, n), (2, n), (7, n)], &[], 0.03, true), Some(2));
        assert_eq!(choose(&here, &[], &[], 0.03, true), None);
    }
}
