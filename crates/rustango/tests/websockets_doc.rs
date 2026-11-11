#![cfg(feature = "sse")]
//! Backing test for `docs/websockets.md` — the broadcast bus, per-user
//! events, the cross-process bus and the WebSocket hub. `sse_pg_live.rs`
//! runs the cross-process bus against PostgreSQL.

use rustango::sse::{EventBus, KeyedEventBus, Recv};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Notification {
    kind: String,
    message: String,
}

fn saved() -> Notification {
    Notification {
        kind: "info".into(),
        message: "Saved".into(),
    }
}

/// "The broadcast bus": `send` returns how many subscribers got it.
#[tokio::test]
async fn the_bus_counts_its_deliveries() {
    let bus: EventBus<Notification> = EventBus::new(100);
    assert_eq!(bus.send(saved()), 0, "no-op with nobody connected");
    let mut rx = bus.subscribe();
    assert_eq!(bus.send(saved()), 1);
    assert_eq!(rx.recv().await.unwrap(), saved());
    assert_eq!(bus.receiver_count(), 1);
}

/// "Per-user events": only the key's subscribers get it, and a lagging one
/// reads `Resync` before the newest events.
#[tokio::test]
async fn per_user_events_reach_only_their_user() {
    let bus: KeyedEventBus<i64, Notification> = KeyedEventBus::new(1);
    let (mut alice, mut bob) = (bus.subscribe(1), bus.subscribe(2));
    bus.send(&1, saved());
    bus.send(&1, saved());
    assert_eq!(alice.recv().await, Some(Recv::Resync));
    assert_eq!(alice.recv().await, Some(Recv::Event(saved())));
    assert_eq!(bus.receiver_count(&2), 1);
    drop(bus);
    assert_eq!(bob.recv().await, None, "`recv` ends once the bus is gone");
}

/// "Across processes": the calls the page shows, compiled.
#[cfg(feature = "postgres")]
#[allow(dead_code)]
async fn across_processes(
    pool: &rustango::sql::Pool,
    user_id: i64,
    notification: &Notification,
) -> Result<(), rustango::sse::BusError> {
    use rustango::sse::PgEventBus;
    // The same secret in every process.
    let secret = rustango::session::SessionSecret::from_bytes(vec![0; 32]);
    let bus: PgEventBus<i64, Notification> =
        PgEventBus::connect(pool, "notifications", 100, &secret).await?;
    let _sub = bus.subscribe(user_id);
    bus.publish(&user_id, notification).await?;
    Ok(())
}

/// "WebSockets": a hub over the bus fans one broadcast out.
#[cfg(feature = "websocket")]
#[tokio::test]
async fn the_hub_broadcasts_over_its_bus() {
    use rustango::ws::WsHub;
    use std::time::Duration;

    #[derive(Clone, Serialize, Deserialize)]
    struct Tick {
        value: i64,
    }

    let bus: EventBus<Tick> = EventBus::new(100);
    let hub = WsHub::new(bus).keepalive(Duration::from_secs(20));
    let mut rx = hub.bus().subscribe();
    assert_eq!(hub.broadcast(Tick { value: 42 }), 1);
    assert_eq!(rx.recv().await.unwrap().value, 42);
}
