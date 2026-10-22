//! Dynamic metrics shared by built-in collectors and out-of-process providers.
mod process;
pub use process::Providers;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

pub const VERSION: u32 = 1;
pub const MAX_METRICS: usize = 256;
pub const HISTORY: usize = 240;
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    /// Resolve relative executable/argument paths against this directory; empty = config directory.
    pub cwd: String,
    pub interval_ms: u64,
    pub heartbeat_ms: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
            args: Vec::new(),
            cwd: String::new(),
            interval_ms: 5000,
            heartbeat_ms: 15000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Number,
    Line,
    State,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetConfig {
    pub metric: String,
    pub view: View,
    pub label: String,
    pub color: String,
    /// Continue subscribing while this widget is hidden.
    pub keep_history: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    #[default]
    Number,
    Boolean,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    /// Local to the provider, e.g. "homepage/latency".
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub value_type: ValueType,
    pub ttl_ms: u64,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Number(f64),
    Boolean(bool),
}

impl Value {
    pub fn number(&self) -> f64 {
        match self {
            Self::Number(n) => *n,
            Self::Boolean(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
    pub fn text(&self, unit: &str) -> String {
        match self {
            Self::Boolean(b) => {
                if *b {
                    "On".into()
                } else {
                    "Off".into()
                }
            }
            Self::Number(n) => format!("{n:.1}{}{}", if unit.is_empty() { "" } else { " " }, unit),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub metric: String,
    pub interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostMessage {
    Hello {
        v: u32,
    },
    /// Replaces the entire subscription set. Empty means pause all collection.
    Subscribe {
        metrics: Vec<Subscription>,
    },
    Stop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ProviderMessage {
    Hello {
        v: u32,
    },
    /// Complete catalog for this session; sent once after hello.
    Describe {
        metrics: Vec<Descriptor>,
    },
    Sample {
        metric: String,
        seq: u64,
        sampled_at_ms: u64,
        value: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    },
    Heartbeat,
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('/')
        && !s.ends_with('/')
        && s.split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
}

#[derive(Clone, Debug)]
pub struct Point {
    pub at_ms: u64,
    pub value: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Metric {
    pub descriptor: Descriptor,
    pub value: Option<Value>,
    pub sampled_at_ms: Option<u64>,
    pub last_success: Option<Instant>,
    pub error: Option<String>,
    pub online: bool,
    pub history: VecDeque<Point>,
    seq: Option<u64>,
    last_sample_ms: Option<u64>,
}

impl Metric {
    pub fn status(&self, now: Instant) -> &'static str {
        if !self.online {
            "offline"
        } else if self.error.is_some() {
            "error"
        } else if self.last_success.map_or(false, |t| {
            now.saturating_duration_since(t) > Duration::from_millis(self.descriptor.ttl_ms)
        }) {
            "stale"
        } else if self.value.is_none() {
            "waiting"
        } else {
            "ok"
        }
    }
    pub fn text(&self) -> String {
        self.value
            .as_ref()
            .map(|v| v.text(&self.descriptor.unit))
            .unwrap_or_else(|| "--".into())
    }

    /// Time-spaced chart columns. Missing/error/expired samples never turn into zeroes.
    pub fn chart(&self, now_ms: u64, window_ms: u64, columns: usize) -> Vec<Option<f32>> {
        if columns == 0 {
            return Vec::new();
        }
        let start = now_ms.saturating_sub(window_ms);
        let (low, high) = self.range();
        let mut out = vec![None; columns];
        let mut points = self.history.iter().peekable();
        let mut last: Option<&Point> = None;
        for (i, slot) in out.iter_mut().enumerate() {
            let at = start.saturating_add(
                window_ms.saturating_mul(i as u64) / columns.saturating_sub(1).max(1) as u64,
            );
            while points.peek().map_or(false, |p| p.at_ms <= at) {
                last = points.next();
            }
            if let Some(p) = last {
                if at.saturating_sub(p.at_ms) <= self.descriptor.ttl_ms {
                    *slot = p
                        .value
                        .map(|v| ((v - low) / (high - low)).clamp(0.0, 1.0) as f32);
                }
            }
        }
        out
    }
    pub fn range(&self) -> (f64, f64) {
        let low = self.descriptor.min.unwrap_or_else(|| {
            self.history
                .iter()
                .filter_map(|p| p.value)
                .fold(0.0, f64::min)
        });
        let high = self.descriptor.max.unwrap_or_else(|| {
            self.history
                .iter()
                .filter_map(|p| p.value)
                .fold(low + 1.0, f64::max)
        });
        (low, if high > low { high } else { low + 1.0 })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub metrics: BTreeMap<String, Metric>,
    pub sources: BTreeMap<String, String>,
}

impl Registry {
    pub fn describe(&mut self, source: &str, descriptors: Vec<Descriptor>) -> Result<(), String> {
        if !valid_id(source) || source.contains('/') || descriptors.len() > MAX_METRICS {
            return Err("invalid source/catalog size".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for d in &descriptors {
            if !valid_id(&d.id)
                || !ids.insert(d.id.clone())
                || d.name.is_empty()
                || d.name.len() > 160
                || d.unit.len() > 24
                || d.ttl_ms == 0
                || d.ttl_ms > 86_400_000
                || d.min.map_or(false, |v| !v.is_finite())
                || d.max.map_or(false, |v| !v.is_finite())
                || matches!((d.min, d.max), (Some(a), Some(b)) if a >= b)
            {
                return Err("invalid metric descriptor".into());
            }
        }
        let prefix = format!("{source}/");
        self.metrics
            .retain(|key, _| !key.starts_with(&prefix) || ids.contains(&key[prefix.len()..]));
        for descriptor in descriptors {
            let key = format!("{source}/{}", descriptor.id);
            if let Some(m) = self.metrics.get_mut(&key) {
                if m.descriptor.value_type != descriptor.value_type
                    || m.descriptor.unit != descriptor.unit
                {
                    m.history.clear();
                    m.value = None;
                    m.last_success = None;
                }
                m.descriptor = descriptor;
                m.online = true;
                m.seq = None;
                m.last_sample_ms = None;
            } else {
                self.metrics.insert(
                    key,
                    Metric {
                        descriptor,
                        value: None,
                        sampled_at_ms: None,
                        last_success: None,
                        error: None,
                        online: true,
                        history: VecDeque::new(),
                        seq: None,
                        last_sample_ms: None,
                    },
                );
            }
        }
        self.sources.insert(source.into(), "online".into());
        Ok(())
    }

    pub fn offline(&mut self, source: &str, reason: &str) {
        let reason: String = reason.chars().take(240).collect();
        if self.sources.get(source) == Some(&reason) {
            return;
        }
        self.sources.insert(source.into(), reason);
        let now = unix_ms();
        let prefix = format!("{source}/");
        for (key, m) in &mut self.metrics {
            if key.starts_with(&prefix) {
                if m.online {
                    push(
                        &mut m.history,
                        Point {
                            at_ms: now,
                            value: None,
                        },
                    );
                }
                m.online = false;
            }
        }
    }

    pub fn sample(
        &mut self,
        source: &str,
        msg: ProviderMessage,
        now: Instant,
    ) -> Result<(), String> {
        let ProviderMessage::Sample {
            metric,
            seq,
            sampled_at_ms,
            value,
            error,
        } = msg
        else {
            return Err("expected sample".into());
        };
        let m = self
            .metrics
            .get_mut(&format!("{source}/{metric}"))
            .ok_or("undeclared metric")?;
        if m.seq.map_or(false, |old| seq <= old)
            || m.last_sample_ms.map_or(false, |old| sampled_at_ms < old)
        {
            return Ok(());
        }
        let wall_now = unix_ms();
        if sampled_at_ms.saturating_sub(wall_now) > 5000
            || wall_now.saturating_sub(sampled_at_ms) > 86_400_000
        {
            return Err("sample timestamp outside allowed clock window".into());
        }
        let good = match (&value, m.descriptor.value_type) {
            (Some(Value::Number(v)), ValueType::Number) => v.is_finite(),
            (Some(Value::Boolean(_)), ValueType::Boolean) => true,
            (None, _) => error.is_some(),
            _ => false,
        };
        if !good
            || (value.is_some() && error.is_some())
            || error.as_ref().map_or(false, |s| s.len() > 1000)
        {
            return Err("invalid sample value/error".into());
        }
        m.seq = Some(seq);
        m.last_sample_ms = Some(sampled_at_ms);
        m.online = true;
        m.error = error;
        push(
            &mut m.history,
            Point {
                at_ms: sampled_at_ms,
                value: value.as_ref().map(Value::number),
            },
        );
        if let Some(value) = value {
            m.value = Some(value);
            m.sampled_at_ms = Some(sampled_at_ms);
            m.last_success = Some(
                now.checked_sub(Duration::from_millis(
                    wall_now.saturating_sub(sampled_at_ms),
                ))
                .unwrap_or(now),
            );
        }
        Ok(())
    }
}

fn push(history: &mut VecDeque<Point>, point: Point) {
    if history.len() == HISTORY {
        history.pop_front();
    }
    history.push_back(point);
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "latency".into(),
            name: "Latency".into(),
            unit: "ms".into(),
            value_type: ValueType::Number,
            ttl_ms: 100,
            min: Some(0.0),
            max: Some(100.0),
        }
    }
    fn sample(seq: u64, at: u64, value: Option<f64>) -> ProviderMessage {
        ProviderMessage::Sample {
            metric: "latency".into(),
            seq,
            sampled_at_ms: at,
            value: value.map(Value::Number),
            error: if value.is_none() {
                Some("timeout".into())
            } else {
                None
            },
        }
    }
    #[test]
    fn freshness_errors_ordering_and_bounded_history() {
        let mut r = Registry::default();
        let now = Instant::now();
        let at = unix_ms();
        r.describe("test", vec![descriptor()]).unwrap();
        assert_eq!(r.metrics["test/latency"].status(now), "waiting");
        r.sample("test", sample(2, at, Some(25.0)), now).unwrap();
        r.sample("test", sample(1, at, Some(99.0)), now).unwrap();
        assert_eq!(r.metrics["test/latency"].text(), "25.0 ms");
        assert_eq!(
            r.metrics["test/latency"].status(now + Duration::from_millis(101)),
            "stale"
        );
        r.sample("test", sample(3, at + 1, None), now).unwrap();
        assert_eq!(r.metrics["test/latency"].status(now), "error");
        assert_eq!(r.metrics["test/latency"].text(), "25.0 ms");
        assert_eq!(
            r.metrics["test/latency"].chart(at + 2, 2, 3),
            vec![Some(0.25), None, None]
        );
        for i in 4..500 {
            r.sample("test", sample(i, at + i, Some(30.0)), now)
                .unwrap();
        }
        assert_eq!(r.metrics["test/latency"].history.len(), HISTORY);
        r.offline("test", "disabled");
        assert_eq!(r.metrics["test/latency"].status(now), "offline");
        r.describe("test", vec![descriptor()]).unwrap();
        r.sample("test", sample(0, at + 600, Some(40.0)), now)
            .unwrap();
        assert_eq!(r.metrics["test/latency"].text(), "40.0 ms");
    }
    #[test]
    fn descriptors_are_atomic_and_values_are_typed() {
        let mut r = Registry::default();
        r.describe("p", vec![descriptor()]).unwrap();
        assert!(r.describe("p", vec![descriptor(), descriptor()]).is_err());
        assert_eq!(r.metrics.len(), 1);
        let m = ProviderMessage::Sample {
            metric: "latency".into(),
            seq: 1,
            sampled_at_ms: unix_ms(),
            value: Some(Value::Boolean(true)),
            error: None,
        };
        assert!(r.sample("p", m, Instant::now()).is_err());
        assert!(!valid_id("../escape"));
        let msg: HostMessage = serde_json::from_str(
            r#"{"t":"subscribe","metrics":[{"metric":"latency","interval_ms":5000}]}"#,
        )
        .unwrap();
        assert!(matches!(msg, HostMessage::Subscribe { .. }));
    }

    #[test]
    fn delayed_samples_do_not_become_fresh_on_receipt_and_silence_breaks_charts() {
        let mut r = Registry::default();
        let now = Instant::now();
        let at = unix_ms();
        r.describe("test", vec![descriptor()]).unwrap();
        r.sample("test", sample(1, at - 1000, Some(25.0)), now)
            .unwrap();
        assert_eq!(r.metrics["test/latency"].status(now), "stale");
        assert_eq!(
            r.metrics["test/latency"].chart(at, 100, 3),
            vec![None, None, None]
        );
        assert!(r
            .sample("test", sample(2, at + 10000, Some(30.0)), now)
            .is_err());
    }
}
