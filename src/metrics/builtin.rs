//! Built-in bindings use the same registry validation, TTL, history and error semantics as
//! process providers. Only completed collection creates samples, never a UI refresh.
use super::MetricsData;
use winstats_metrics::{Descriptor, ProviderMessage, Registry, Value, ValueType};

pub(super) fn describe(registry: &mut Registry, interval: u32) {
    let ttl_ms = (interval as u64 * 3).clamp(1000, 86_400_000);
    if registry
        .metrics
        .get("builtin/cpu/usage")
        .map_or(false, |m| m.descriptor.ttl_ms == ttl_ms)
    {
        return;
    }
    let descriptors = [
        ("cpu/usage", "CPU", "%", Some(100.0)),
        ("memory/used", "Memory used", "B", None),
        ("memory/usage", "Memory usage", "%", Some(100.0)),
        ("network/rx", "Network download", "B/s", None),
        ("network/tx", "Network upload", "B/s", None),
        ("disk/read", "Disk read", "B/s", None),
        ("disk/write", "Disk write", "B/s", None),
        ("gpu/usage", "GPU", "%", Some(100.0)),
        ("battery/level", "Battery", "%", Some(100.0)),
    ]
    .into_iter()
    .map(|(id, name, unit, max)| Descriptor {
        id: id.into(),
        name: name.into(),
        unit: unit.into(),
        value_type: ValueType::Number,
        ttl_ms,
        min: Some(0.0),
        max,
    })
    .collect();
    registry
        .describe("builtin", descriptors)
        .expect("valid built-in descriptors");
}

pub(super) fn publish(
    data: &mut MetricsData,
    seq: u64,
    at: u64,
    interval: u32,
    disk_available: bool,
) {
    describe(&mut data.generic, interval);
    let values = [
        ("cpu/usage", Some(data.cpu_total as f64 * 100.0)),
        (
            "memory/used",
            (data.mem.total > 0).then_some(data.mem.used as f64),
        ),
        (
            "memory/usage",
            (data.mem.total > 0).then_some(data.mem.pct as f64 * 100.0),
        ),
        ("network/rx", Some(data.net_rx)),
        ("network/tx", Some(data.net_tx)),
        ("disk/read", disk_available.then_some(data.disk_read)),
        ("disk/write", disk_available.then_some(data.disk_write)),
        ("gpu/usage", data.gpu.map(|v| v as f64 * 100.0)),
        (
            "battery/level",
            data.battery.map(|b| b.level as f64 * 100.0),
        ),
    ];
    for (metric, value) in values {
        let value = value.filter(|v| v.is_finite());
        let msg = ProviderMessage::Sample {
            metric: metric.into(),
            seq,
            sampled_at_ms: at,
            value: value.map(Value::Number),
            error: value
                .is_none()
                .then(|| "not available from the local collector".into()),
        };
        if let Err(e) = data
            .generic
            .sample("builtin", msg, std::time::Instant::now())
        {
            crate::log!("builtin {metric}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn builtins_match_snapshot_and_use_shared_error_ttl_and_history_semantics() {
        let mut d = MetricsData {
            cpu_total: 0.25,
            net_rx: 123.0,
            net_tx: 456.0,
            disk_read: 789.0,
            disk_write: 12.0,
            gpu: Some(0.5),
            ..Default::default()
        };
        d.mem.total = 1000;
        d.mem.used = 500;
        d.mem.pct = 0.5;
        let at = winstats_metrics::unix_ms();
        publish(&mut d, 1, at, 250, true);
        for (id, value) in [
            ("cpu/usage", 25.0),
            ("memory/used", 500.0),
            ("memory/usage", 50.0),
            ("network/rx", 123.0),
            ("network/tx", 456.0),
            ("disk/read", 789.0),
            ("disk/write", 12.0),
            ("gpu/usage", 50.0),
        ] {
            assert_eq!(
                d.generic.metrics[&format!("builtin/{id}")].value,
                Some(Value::Number(value))
            );
        }
        let gpu = &d.generic.metrics["builtin/gpu/usage"];
        assert_eq!(gpu.status(Instant::now() + Duration::from_secs(2)), "stale");
        assert_eq!(
            d.generic.metrics["builtin/battery/level"].status(Instant::now()),
            "error"
        );
        d.gpu = None;
        publish(&mut d, 2, at + 1, 250, false);
        let gpu = &d.generic.metrics["builtin/gpu/usage"];
        assert_eq!(gpu.status(Instant::now()), "error");
        assert_eq!(gpu.value, Some(Value::Number(50.0)));
        assert!(gpu.history.back().unwrap().value.is_none());
        assert_eq!(
            d.generic.metrics["builtin/disk/read"].status(Instant::now()),
            "error"
        );
        d.gpu = Some(0.25);
        publish(&mut d, 3, at + 2, 250, true);
        assert_eq!(
            d.generic.metrics["builtin/gpu/usage"].status(Instant::now()),
            "ok"
        );
        // Same sequence is not a new sample, even when a UI refresh reads it again.
        publish(&mut d, 3, at + 2, 250, true);
        assert_eq!(d.generic.metrics["builtin/gpu/usage"].history.len(), 3);
        for n in 4..400 {
            publish(&mut d, n, at + n, 250, true);
        }
        assert_eq!(
            d.generic.metrics["builtin/cpu/usage"].history.len(),
            winstats_metrics::HISTORY
        );
        publish(&mut d, 400, at + 400, 2000, true);
        assert_eq!(
            d.generic.metrics["builtin/cpu/usage"].descriptor.ttl_ms,
            6000
        );
    }
}
