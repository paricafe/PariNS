use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;

fn peer(last: u8) -> IpAddr {
    IpAddr::from([192, 0, 2, last])
}

fn enabled() -> Settings {
    Settings {
        enabled: true,
        rate_per_sec: 2,
        burst: 2,
        ..Settings::default()
    }
}

fn query_at(limiter: &Arc<Limiter>, peer: IpAddr, now: Instant) -> Result<Permit, Denied> {
    limiter.acquire_at(peer, Kind::Query, now)
}

#[test]
fn burst_refills_fractionally_and_drop_never_refunds_rate() {
    let limiter = Arc::new(Limiter::new(&enabled()).unwrap());
    let now = Instant::now();
    drop(query_at(&limiter, peer(1), now).unwrap());
    drop(query_at(&limiter, peer(1), now).unwrap());
    assert_eq!(query_at(&limiter, peer(1), now).err(), Some(Denied::Rate));
    assert_eq!(
        query_at(&limiter, peer(1), now + Duration::from_millis(499)).err(),
        Some(Denied::Rate)
    );
    drop(query_at(&limiter, peer(1), now + Duration::from_millis(500)).unwrap());
    assert_eq!(
        query_at(&limiter, peer(1), now + Duration::from_millis(999)).err(),
        Some(Denied::Rate)
    );
    drop(query_at(&limiter, peer(1), now + Duration::from_secs(1)).unwrap());
}

#[test]
fn source_and_configured_subnet_budgets_are_distinct() {
    let settings = Settings {
        burst: 1,
        ..enabled()
    };
    let limiter = Arc::new(Limiter::new(&settings).unwrap());
    drop(limiter.try_query(peer(1)).unwrap());
    assert_eq!(limiter.try_query(peer(1)).err(), Some(Denied::Rate));
    drop(limiter.try_query(peer(2)).unwrap());

    let limiter = Arc::new(
        Limiter::new(&Settings {
            ipv4_prefix: 24,
            ..settings
        })
        .unwrap(),
    );
    drop(limiter.try_query(peer(1)).unwrap());
    assert_eq!(limiter.try_query(peer(2)).err(), Some(Denied::Rate));
    drop(limiter.try_query("192.0.3.1".parse().unwrap()).unwrap());
}

#[test]
fn mapped_ipv4_cannot_bypass_ipv4_budget_and_ipv6_uses_prefix() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            burst: 1,
            ..enabled()
        })
        .unwrap(),
    );
    drop(limiter.try_query(peer(1)).unwrap());
    assert_eq!(
        limiter.try_query("::ffff:192.0.2.1".parse().unwrap()).err(),
        Some(Denied::Rate)
    );
    drop(limiter.try_query("2001:db8:1::1".parse().unwrap()).unwrap());
    assert_eq!(
        limiter.try_query("2001:db8:1::2".parse().unwrap()).err(),
        Some(Denied::Rate)
    );
    drop(limiter.try_query("2001:db8:2::1".parse().unwrap()).unwrap());
}

#[test]
fn drop_releases_independent_query_and_connection_quotas() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_inflight: 1,
            max_connections: 1,
            ..enabled()
        })
        .unwrap(),
    );
    let query = limiter.try_query(peer(1)).unwrap();
    let connection = limiter.try_connection(peer(1)).unwrap();
    assert_eq!(limiter.try_query(peer(1)).err(), Some(Denied::Inflight));
    assert_eq!(
        limiter.try_connection(peer(1)).err(),
        Some(Denied::Connections)
    );
    drop(query);
    drop(limiter.try_query(peer(1)).unwrap());
    drop(connection);
    drop(limiter.try_connection(peer(1)).unwrap());
    assert_eq!(limiter.try_query(peer(1)).err(), Some(Denied::Rate));
}

#[test]
fn full_table_preserves_token_debt_and_active_permits() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_sources: 1,
            burst: 1,
            rate_per_sec: 1,
            ..enabled()
        })
        .unwrap(),
    );
    let now = Instant::now();
    let active = query_at(&limiter, peer(1), now).unwrap();
    assert_eq!(
        query_at(&limiter, peer(2), now + Duration::from_secs(2)).err(),
        Some(Denied::TableFull)
    );
    drop(active);
    // Use a separate table to check debt independently of the full refill above.
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_sources: 1,
            burst: 1,
            rate_per_sec: 1,
            ..enabled()
        })
        .unwrap(),
    );
    drop(query_at(&limiter, peer(1), now).unwrap());
    assert_eq!(
        query_at(&limiter, peer(2), now + Duration::from_millis(999)).err(),
        Some(Denied::TableFull)
    );
    assert_eq!(
        query_at(&limiter, peer(1), now + Duration::from_millis(999)).err(),
        Some(Denied::Rate)
    );
    drop(query_at(&limiter, peer(2), now + Duration::from_secs(1)).unwrap());
    assert_eq!(
        query_at(&limiter, peer(1), now + Duration::from_secs(1)).err(),
        Some(Denied::TableFull)
    );
}

#[test]
fn disabled_limiter_has_no_source_state_or_quotas() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_sources: 1,
            max_inflight: 1,
            max_connections: 1,
            burst: 1,
            ..Settings::default()
        })
        .unwrap(),
    );
    let mut permits = Vec::new();
    for source in 1..100 {
        permits.push(limiter.try_query(peer(source)).unwrap());
        permits.push(limiter.try_connection(peer(source)).unwrap());
    }
    assert!(limiter.state.is_none());
}

#[test]
fn settings_reject_zero_excessive_and_unknown_values() {
    assert!(Settings::default().validate().is_ok());
    for field in [
        "rate_per_sec",
        "burst",
        "max_sources",
        "max_inflight",
        "max_connections",
    ] {
        for value in [0, 1_000_001] {
            let settings: Settings = toml::from_str(&format!("{field} = {value}")).unwrap();
            assert!(settings.validate().is_err(), "{field}={value}");
        }
    }
    for field in ["max_sources", "max_inflight", "max_connections"] {
        assert!(
            toml::from_str::<Settings>(&format!("{field} = 65537"))
                .unwrap()
                .validate()
                .is_err()
        );
    }
    assert!(
        Settings {
            ipv4_prefix: 33,
            ..Settings::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Settings {
            ipv6_prefix: 129,
            ..Settings::default()
        }
        .validate()
        .is_err()
    );
    assert!(toml::from_str::<Settings>("unexpected = true").is_err());
}

#[test]
fn simultaneous_callers_cannot_exceed_concurrent_budget() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_inflight: 3,
            rate_per_sec: 100,
            burst: 100,
            ..enabled()
        })
        .unwrap(),
    );
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let limiter = limiter.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let permit = limiter.try_query(peer(1));
                barrier.wait();
                permit
            })
        })
        .collect();
    barrier.wait();
    barrier.wait();
    let permits: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(permits.iter().filter(|permit| permit.is_ok()).count(), 3);
    assert!(
        permits
            .iter()
            .filter_map(|permit| permit.as_ref().err())
            .all(|denied| *denied == Denied::Inflight)
    );
    drop(permits);
    assert!(limiter.try_query(peer(1)).is_ok());
}

#[test]
fn bounded_sweep_eventually_finds_inactive_entries_without_evicting_active_ones() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_sources: 17,
            ..enabled()
        })
        .unwrap(),
    );
    let now = Instant::now();
    let connections: Vec<_> = (1..=16)
        .map(|source| {
            limiter
                .acquire_at(peer(source), Kind::Connection, now)
                .unwrap()
        })
        .collect();
    drop(query_at(&limiter, peer(17), now).unwrap());
    let refilled = now + Duration::from_secs(1);
    // First pass checks only the sixteen active entries, then advances its cursor.
    assert_eq!(
        query_at(&limiter, peer(18), refilled).err(),
        Some(Denied::TableFull)
    );
    drop(query_at(&limiter, peer(18), refilled).unwrap());
    let state = limiter.state.as_ref().unwrap().lock().unwrap();
    assert_eq!(state.entries.len(), 17);
    assert_eq!(state.sweep.len(), 17);
    assert!(!state.entries.contains_key(&limiter.source(peer(17))));
    assert!(state.entries.contains_key(&limiter.source(peer(18))));
    drop(state);
    drop(connections);
}

#[tokio::test]
async fn cancelling_a_task_releases_permits_but_preserves_rate_debt() {
    let limiter = Arc::new(
        Limiter::new(&Settings {
            max_inflight: 1,
            max_connections: 1,
            rate_per_sec: 1,
            burst: 1,
            ..enabled()
        })
        .unwrap(),
    );
    let now = Instant::now();
    let (ready, acquired) = tokio::sync::oneshot::channel();
    let task_limiter = limiter.clone();
    let task = tokio::spawn(async move {
        let _query = query_at(&task_limiter, peer(1), now).unwrap();
        let _connection = task_limiter.try_connection(peer(1)).unwrap();
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    acquired.await.unwrap();
    assert_eq!(
        limiter.try_connection(peer(1)).err(),
        Some(Denied::Connections)
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(limiter.try_connection(peer(1)).unwrap());
    assert_eq!(query_at(&limiter, peer(1), now).err(), Some(Denied::Rate));
    drop(query_at(&limiter, peer(1), now + Duration::from_secs(1)).unwrap());
}

#[test]
fn long_idle_refill_caps_burst_and_stale_timestamps_never_mint_tokens() {
    let limiter = Arc::new(Limiter::new(&enabled()).unwrap());
    let now = Instant::now();
    drop(query_at(&limiter, peer(1), now).unwrap());
    drop(query_at(&limiter, peer(1), now).unwrap());
    let later = now + Duration::from_secs(3600);
    drop(query_at(&limiter, peer(1), later).unwrap());
    drop(query_at(&limiter, peer(1), later).unwrap());
    assert_eq!(query_at(&limiter, peer(1), now).err(), Some(Denied::Rate));
    assert_eq!(query_at(&limiter, peer(1), later).err(), Some(Denied::Rate));
}
