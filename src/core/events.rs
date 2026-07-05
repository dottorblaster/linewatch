use super::types::Status;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AgcomCategory {
    /// At least one Down sample was observed.
    CompleteInterruption,
    /// All samples were Degraded / DnsFail / HttpFail (no Down).
    IrregularService,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutageEvent {
    pub started: OffsetDateTime,
    pub ended: Option<OffsetDateTime>,
    pub worst_status: Status,
    pub min_temp_c: Option<f64>,
    pub samples_count: u32,
    pub category: AgcomCategory,
}

#[derive(Debug, Clone)]
pub struct DebounceCfg {
    /// Number of consecutive non-Ok/non-LocalOrPower samples before opening.
    pub open_after: u32,
    /// Number of consecutive Ok samples before closing.
    pub close_after: u32,
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum MachineState {
    Idle {
        bad_count: u32,
        streak_started: OffsetDateTime,
        streak_worst: Status,
        streak_min_temp: Option<f64>,
    },
    Open {
        close_count: u32,
        started: OffsetDateTime,
        worst_status: Status,
        min_temp_c: Option<f64>,
        samples_count: u32,
    },
}

#[derive(Debug, Clone)]
pub struct Machine {
    state: MachineState,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            state: MachineState::Idle {
                bad_count: 0,
                // Placeholder – will be overwritten before first use.
                streak_started: OffsetDateTime::UNIX_EPOCH,
                streak_worst: Status::Ok,
                streak_min_temp: None,
            },
        }
    }
}

impl Machine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one sample into the state machine.
    ///
    /// Returns `Some(OutageEvent)` **only** when a previously opened event
    /// closes after `close_after` consecutive `Ok` samples.
    pub fn advance(
        &mut self,
        ts: OffsetDateTime,
        status: Status,
        temp_c: Option<f64>,
        cfg: &DebounceCfg,
    ) -> Option<OutageEvent> {
        // Take ownership of the old state so we can mutate self freely.
        let old_state = std::mem::replace(
            &mut self.state,
            MachineState::Idle {
                bad_count: 0,
                streak_started: OffsetDateTime::UNIX_EPOCH,
                streak_worst: Status::Ok,
                streak_min_temp: None,
            },
        );

        match old_state {
            MachineState::Idle {
                mut bad_count,
                mut streak_started,
                mut streak_worst,
                mut streak_min_temp,
            } => {
                match status {
                    Status::Ok | Status::LocalOrPower => {
                        bad_count = 0;
                    }
                    _ => {
                        if bad_count == 0 {
                            streak_started = ts;
                            streak_worst = status;
                            streak_min_temp = temp_c;
                        } else {
                            streak_worst = most_severe(&streak_worst, &status);
                            streak_min_temp = min_opt(streak_min_temp, temp_c);
                        }
                        bad_count += 1;

                        if bad_count >= cfg.open_after {
                            self.state = MachineState::Open {
                                close_count: 0,
                                started: streak_started,
                                worst_status: streak_worst,
                                min_temp_c: streak_min_temp,
                                samples_count: bad_count,
                            };
                            return None;
                        }
                    }
                }
                self.state = MachineState::Idle {
                    bad_count,
                    streak_started,
                    streak_worst,
                    streak_min_temp,
                };
                None
            }

            MachineState::Open {
                mut close_count,
                started,
                mut worst_status,
                mut min_temp_c,
                mut samples_count,
            } => {
                let result = match status {
                    Status::Ok => {
                        close_count += 1;
                        samples_count += 1;
                        min_temp_c = min_opt(min_temp_c, temp_c);

                        if close_count >= cfg.close_after {
                            Some(OutageEvent {
                                started,
                                ended: Some(ts),
                                worst_status: worst_status.clone(),
                                min_temp_c,
                                samples_count,
                                category: category(&worst_status),
                            })
                        } else {
                            None
                        }
                    }
                    _ => {
                        close_count = 0;
                        samples_count += 1;
                        min_temp_c = min_opt(min_temp_c, temp_c);

                        if severity(&status) > severity(&worst_status) {
                            worst_status = status;
                        }
                        None
                    }
                };

                // Only persist Open state if the event is not closing.
                if result.is_none() {
                    self.state = MachineState::Open {
                        close_count,
                        started,
                        worst_status,
                        min_temp_c,
                        samples_count,
                    };
                }
                result
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Severity ranking used for `worst_status`. Higher = worse.
fn severity(s: &Status) -> u8 {
    match s {
        Status::Down => 4,
        Status::DnsFail => 3,
        Status::HttpFail => 2,
        Status::Degraded => 1,
        // These two should never be the worst during an event.
        Status::LocalOrPower | Status::Ok => 0,
    }
}

/// Pick the more severe of two statuses.
fn most_severe(a: &Status, b: &Status) -> Status {
    if severity(a) >= severity(b) {
        a.clone()
    } else {
        b.clone()
    }
}

fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn category(s: &Status) -> AgcomCategory {
    match s {
        Status::Down => AgcomCategory::CompleteInterruption,
        _ => AgcomCategory::IrregularService,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::types::Status;
    use super::*;
    use time::macros::datetime;

    fn cfg(open: u32, close: u32) -> DebounceCfg {
        DebounceCfg {
            open_after: open,
            close_after: close,
        }
    }

    fn ts(hour: u8, min: u8, sec: u8) -> OffsetDateTime {
        datetime!(2026-07-05 00:00:00 UTC)
            .replace_time(time::Time::from_hms(hour, min, sec).unwrap())
    }

    /// Feed a sequence of statuses into a fresh Machine and collect events.
    fn run(statuses: &[Status], open_after: u32, close_after: u32) -> Vec<OutageEvent> {
        let mut m = Machine::new();
        let cfg = cfg(open_after, close_after);
        let mut events = Vec::new();
        for (i, s) in statuses.iter().enumerate() {
            let t = ts(0, 0, i as u8);
            if let Some(ev) = m.advance(t, s.clone(), None, &cfg) {
                events.push(ev);
            }
        }
        events
    }

    // -- 1. Single blip below open_after → no event --------------------------

    #[test]
    fn single_blip_does_not_open() {
        let events = run(&[Status::Degraded], 3, 2);
        assert_eq!(events.len(), 0);
    }

    // -- 2. Sustained down ---------------------------------------------------

    #[test]
    fn sustained_down_opens_and_closes() {
        // 3 downs open → then 2 oks close
        let events = run(
            &[
                Status::Down,
                Status::Down,
                Status::Down, // opens at this sample
                Status::Ok,
                Status::Ok, // closes at this sample
            ],
            3,
            2,
        );
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.worst_status, Status::Down);
        assert_eq!(ev.category, AgcomCategory::CompleteInterruption);
        assert_eq!(ev.samples_count, 5);
        assert!(ev.ended.is_some());
    }

    #[test]
    fn sustained_down_even_longer() {
        // 4 downs, 4 more downs, then 2 oks
        let events = run(
            &[
                Status::Down,
                Status::Down,
                Status::Down,
                Status::Down, // opens at #3 (0-indexed)
                Status::Down,
                Status::Down,
                Status::Down,
                Status::Down,
                Status::Ok,
                Status::Ok, // closes
            ],
            4,
            2,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].samples_count, 10);
    }

    // -- 3. Degraded → Down escalation --------------------------------------

    #[test]
    fn degraded_then_down_escalates_to_complete_interruption() {
        let events = run(
            &[
                Status::Degraded,
                Status::Degraded, // opens after 2 degraded (open_after=2)
                Status::Down,     // escalates
                Status::Ok,
                Status::Ok, // closes
            ],
            2,
            2,
        );
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        // worst_status should have escalated to Down
        assert_eq!(ev.worst_status, Status::Down);
        assert_eq!(ev.category, AgcomCategory::CompleteInterruption);
        assert_eq!(ev.samples_count, 5);
    }

    #[test]
    fn degraded_only_stays_irregular_service() {
        let events = run(
            &[
                Status::Degraded,
                Status::Degraded, // opens
                Status::Degraded,
                Status::Degraded,
                Status::Ok,
                Status::Ok, // closes
            ],
            2,
            2,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].worst_status, Status::Degraded);
        assert_eq!(events[0].category, AgcomCategory::IrregularService);
    }

    // -- 4. Flapping at the boundary -----------------------------------------

    #[test]
    fn flapping_below_open_never_opens() {
        // Degraded, Ok, Degraded, Ok – never 3 in a row
        let events = run(
            &[
                Status::Degraded,
                Status::Degraded,
                Status::Ok, // resets
                Status::Degraded,
                Status::Degraded,
                Status::Ok, // resets
            ],
            3,
            2,
        );
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn close_resets_then_opens_again() {
        // 3 downs → opens, 1 ok (not enough), 1 down (resets close counter),
        // 2 more oks → finally closes.
        let mut m = Machine::new();
        let cfg = cfg(3, 2);
        let times = [
            ts(0, 0, 0),
            ts(0, 0, 1),
            ts(0, 0, 2), // 3 downs → opens
            ts(0, 0, 3), // 1st Ok (close_count=1)
            ts(0, 0, 4), // Down resets close_count to 0
            ts(0, 0, 5),
            ts(0, 0, 6), // 2 Oks → closes
        ];
        let statuses = [
            Status::Down,
            Status::Down,
            Status::Down,
            Status::Ok,
            Status::Down,
            Status::Ok,
            Status::Ok,
        ];
        let mut events = Vec::new();
        for (t, s) in times.iter().zip(statuses.iter()) {
            if let Some(ev) = m.advance(*t, s.clone(), None, &cfg) {
                events.push(ev);
            }
        }
        assert_eq!(events.len(), 1);
        // The Down at t4 reset the close counter, so the event spans all 7
        // samples (3 open + 1 Ok + 1 Down + 2 Ok).
        assert_eq!(events[0].samples_count, 7);
        assert_eq!(events[0].worst_status, Status::Down);
        assert_eq!(events[0].ended, Some(ts(0, 0, 6)));
    }

    // -- 5. Multiple events --------------------------------------------------

    #[test]
    fn two_separate_events() {
        let events = run(
            &[
                Status::Down,
                Status::Down,
                Status::Down, // open
                Status::Ok,
                Status::Ok, // close event 1
                Status::Degraded,
                Status::Degraded,
                Status::Degraded, // open
                Status::Ok,
                Status::Ok, // close event 2
            ],
            3,
            2,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].worst_status, Status::Down);
        assert_eq!(events[0].category, AgcomCategory::CompleteInterruption);
        assert_eq!(events[1].worst_status, Status::Degraded);
        assert_eq!(events[1].category, AgcomCategory::IrregularService);
    }

    // -- 6. Temperature tracking --------------------------------------------

    #[test]
    fn tracks_min_temperature() {
        let mut m = Machine::new();
        let cfg = cfg(2, 2);
        let times = [ts(0, 0, 0), ts(0, 0, 1), ts(0, 0, 2), ts(0, 0, 3)];
        let statuses = [Status::Down, Status::Down, Status::Ok, Status::Ok];
        let temps = [Some(30.0), Some(28.5), Some(27.0), Some(29.0)];
        let mut events = Vec::new();
        for (t, (s, tmp)) in times.iter().zip(statuses.iter().zip(temps.iter())) {
            if let Some(ev) = m.advance(*t, s.clone(), *tmp, &cfg) {
                events.push(ev);
            }
        }
        assert_eq!(events.len(), 1);
        // Min across all samples: 27.0
        assert!((events[0].min_temp_c.unwrap() - 27.0).abs() < f64::EPSILON);
    }
}
