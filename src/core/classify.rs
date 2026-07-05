use super::types::{ProbeOutcome, Sample, Status, TargetKind, Thresholds};

/// Classify the status of a sample according to a strict precedence.
///
/// 1. Gateway outcome unreachable → `LocalOrPower`.
/// 2. Else if every external anchor (IcmpAnchor / TcpAnchor) unreachable → `Down`.
/// 3. Else if Dns outcome unreachable → `DnsFail`.
/// 4. Else if Http outcome unreachable → `HttpFail`.
/// 5. Else if any reachable outcome exceeds `max_loss_pct` or `max_rtt_ms` → `Degraded`.
/// 6. Else → `Ok`.
pub fn classify(sample: &Sample, t: &Thresholds) -> Status {
    // 1. Gateway unreachable → LocalOrPower
    if sample
        .outcomes
        .iter()
        .any(|o| o.kind == TargetKind::Gateway && !o.reachable)
    {
        return Status::LocalOrPower;
    }

    // 2. Every external anchor unreachable → Down
    let anchors: Vec<&ProbeOutcome> = sample
        .outcomes
        .iter()
        .filter(|o| o.kind == TargetKind::IcmpAnchor || o.kind == TargetKind::TcpAnchor)
        .collect();

    if !anchors.is_empty() && anchors.iter().all(|o| !o.reachable) {
        return Status::Down;
    }

    // 3. Dns unreachable → DnsFail
    if sample
        .outcomes
        .iter()
        .any(|o| o.kind == TargetKind::Dns && !o.reachable)
    {
        return Status::DnsFail;
    }

    // 4. Http unreachable → HttpFail
    if sample
        .outcomes
        .iter()
        .any(|o| o.kind == TargetKind::Http && !o.reachable)
    {
        return Status::HttpFail;
    }

    // 5. Any reachable outcome exceeds thresholds → Degraded
    if sample.outcomes.iter().any(|o| {
        o.reachable
            && (o.loss_pct > t.max_loss_pct
                || o.rtt
                    .map(|rtt| rtt.as_millis() > t.max_rtt_ms.into())
                    .unwrap_or(false))
    }) {
        return Status::Degraded;
    }

    // 6. All good
    Status::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use time::macros::datetime;

    fn default_thresholds() -> Thresholds {
        Thresholds {
            max_loss_pct: 10,
            max_rtt_ms: 200,
        }
    }

    fn sample_with_outcomes(outcomes: Vec<ProbeOutcome>) -> Sample {
        Sample {
            ts: datetime!(2026-07-05 12:00:00 UTC),
            temp_c: None,
            outcomes,
        }
    }

    fn outcome(kind: TargetKind, reachable: bool) -> ProbeOutcome {
        ProbeOutcome {
            kind,
            reachable,
            rtt: Some(Duration::from_millis(100)),
            loss_pct: 0,
        }
    }

    // -- 1. Gateway unreachable → LocalOrPower --

    #[test]
    fn gateway_unreachable_returns_local_or_power() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, false),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, true),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::LocalOrPower);
    }

    #[test]
    fn gateway_unreachable_takes_precedence_over_down() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, false),
            outcome(TargetKind::IcmpAnchor, false),
            outcome(TargetKind::TcpAnchor, false),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::LocalOrPower);
    }

    // -- 2. All anchors unreachable → Down --

    #[test]
    fn all_anchors_unreachable_returns_down() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, false),
            outcome(TargetKind::TcpAnchor, false),
            outcome(TargetKind::Dns, true),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Down);
    }

    #[test]
    fn single_anchor_unreachable_returns_down() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, false),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Down);
    }

    #[test]
    fn one_anchor_reachable_prevents_down() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::TcpAnchor, false),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Ok);
    }

    #[test]
    fn no_anchors_does_not_trigger_down() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::Dns, true),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Ok);
    }

    // -- 3. Dns unreachable → DnsFail --

    #[test]
    fn dns_unreachable_returns_dns_fail() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, false),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::DnsFail);
    }

    #[test]
    fn dns_fail_takes_precedence_over_http_fail() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, false),
            outcome(TargetKind::Http, false),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::DnsFail);
    }

    // -- 4. Http unreachable → HttpFail --

    #[test]
    fn http_unreachable_returns_http_fail() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, true),
            outcome(TargetKind::Http, false),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::HttpFail);
    }

    // -- 5. Exceeds thresholds → Degraded --

    #[test]
    fn loss_exceeds_max_returns_degraded() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, true),
            ProbeOutcome {
                kind: TargetKind::Http,
                reachable: true,
                rtt: Some(Duration::from_millis(50)),
                loss_pct: 50,
            },
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Degraded);
    }

    #[test]
    fn rtt_exceeds_max_returns_degraded() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, true),
            ProbeOutcome {
                kind: TargetKind::Http,
                reachable: true,
                rtt: Some(Duration::from_millis(500)),
                loss_pct: 0,
            },
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Degraded);
    }

    #[test]
    fn no_rtt_does_not_trigger_rtt_threshold() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            ProbeOutcome {
                kind: TargetKind::Dns,
                reachable: true,
                rtt: None,
                loss_pct: 0,
            },
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Ok);
    }

    // -- 6. All good → Ok --

    #[test]
    fn all_good_returns_ok() {
        let s = sample_with_outcomes(vec![
            outcome(TargetKind::Gateway, true),
            outcome(TargetKind::IcmpAnchor, true),
            outcome(TargetKind::Dns, true),
            outcome(TargetKind::Http, true),
        ]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Ok);
    }

    #[test]
    fn single_outcome_ok() {
        let s = sample_with_outcomes(vec![outcome(TargetKind::Gateway, true)]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Ok);
    }

    #[test]
    fn empty_outcomes_returns_ok() {
        let s = sample_with_outcomes(vec![]);
        assert_eq!(classify(&s, &default_thresholds()), Status::Ok);
    }
}
