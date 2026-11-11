// Copyright 2026 PRAGMA
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

#[cfg(not(target_arch = "wasm32"))]
use opentelemetry::KeyValue;

#[cfg(not(target_arch = "wasm32"))]
use crate::{Counter, Gauge};
use crate::{Meter, MetricRecorder, MetricsEvent};

/// Block production: whether the node forges, and what became of the slots it leads.
///
/// Where cardano-node reports the same thing its metric name is used, so that existing stake pool
/// dashboards and alerts apply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ForgeMetrics {
    /// Whether the node holds forging credentials. Every node reports it.
    Enabled(bool),
    /// Led slots still ahead in the leader schedules computed so far.
    ScheduledSlots(u64),
    /// A block was forged for a led slot.
    Forged,
    /// A led slot was not forged. cardano-node's `slotsMissed` counts missed slot leader checks;
    /// Amaru computes its leader schedule ahead of time, so it counts the led slots it did not forge.
    MissedSlot { reason: String },
}

#[cfg(target_arch = "wasm32")]
impl MetricRecorder for ForgeMetrics {
    fn record_to_meter(&self, _meter: &Meter) {}
}

#[cfg(not(target_arch = "wasm32"))]
impl MetricRecorder for ForgeMetrics {
    fn record_to_meter(&self, meter: &Meter) {
        static FORGING_ENABLED: OnceLock<Gauge<u64>> = OnceLock::new();
        static FORGED: OnceLock<Counter<u64>> = OnceLock::new();
        static SLOTS_MISSED: OnceLock<Counter<u64>> = OnceLock::new();
        static MISSED_SLOT_TOTAL: OnceLock<Counter<u64>> = OnceLock::new();
        static SCHEDULED_SLOTS: OnceLock<Gauge<u64>> = OnceLock::new();

        let Some(meter) = meter.get() else {
            return;
        };

        // Metrics common to amaru and cardano-node

        let forging_enabled = FORGING_ENABLED.get_or_init(|| {
            meter
                .u64_gauge("cardano_node_metrics_forging_enabled_int")
                .with_description("1 if the node holds forging credentials, 0 otherwise")
                .with_unit("int")
                .build()
        });

        let forged = FORGED.get_or_init(|| {
            meter
                .u64_counter("cardano_node_metrics_Forge_forged_counter")
                .with_description("number of blocks forged")
                .build()
        });

        let slots_missed = SLOTS_MISSED.get_or_init(|| {
            meter
                .u64_counter("cardano_node_metrics_slotsMissed_int")
                .with_description("number of led slots that were not forged")
                .with_unit("int")
                .build()
        });

        // Amaru-specific metrics

        let missed_slot_total = MISSED_SLOT_TOTAL.get_or_init(|| {
            meter
                .u64_counter("amaru_consensus_forge_missed_slot_total")
                .with_description("number of led slots that were not forged, labelled by reason")
                .build()
        });

        let scheduled_slots = SCHEDULED_SLOTS.get_or_init(|| {
            meter
                .u64_gauge("amaru_consensus_forge_scheduled_slots")
                .with_description("led slots still ahead in the leader schedules computed so far")
                .build()
        });

        match self {
            ForgeMetrics::Enabled(enabled) => {
                forging_enabled.record(u64::from(*enabled), &[]);
                if *enabled {
                    // Start the outcome counters at zero: a series that first appears at one shows
                    // no increase, so the first forged block or missed slot would go unnoticed.
                    forged.add(0, &[]);
                    slots_missed.add(0, &[]);
                }
            }
            ForgeMetrics::ScheduledSlots(slots) => scheduled_slots.record(*slots, &[]),
            ForgeMetrics::Forged => forged.add(1, &[]),
            ForgeMetrics::MissedSlot { reason } => {
                slots_missed.add(1, &[]);
                missed_slot_total.add(1, &[KeyValue::new("reason", reason.to_string())]);
            }
        }
    }
}

impl From<ForgeMetrics> for MetricsEvent {
    fn from(value: ForgeMetrics) -> Self {
        MetricsEvent::ForgeMetrics(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All cases share one test because the instruments live in process-wide `OnceLock`s, bound to
    /// the meter that first initialises them.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn exports_forging_state_and_slot_outcomes() {
        use opentelemetry::metrics::MeterProvider;
        use opentelemetry_sdk::metrics::{
            InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
            data::{AggregatedMetrics, MetricData},
        };

        fn reason<'a>(mut attributes: impl Iterator<Item = &'a KeyValue>) -> String {
            attributes
                .find(|attribute| attribute.key.as_str() == "reason")
                .map(|attribute| attribute.value.to_string())
                .unwrap_or_default()
        }

        let exporter = InMemoryMetricExporter::default();
        let provider =
            SdkMeterProvider::builder().with_reader(PeriodicReader::builder(exporter.clone()).build()).build();
        let meter = Meter::from(provider.meter("amaru"));

        // A value per series, keyed by metric name and `reason` label.
        let export = || {
            provider.force_flush().expect("flush metrics");
            let exported = exporter.get_finished_metrics().expect("finished metrics");
            exporter.reset();
            let mut series: Vec<(String, String, u64)> = exported
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .flat_map(|metric| {
                    let points: Vec<(String, u64)> =
                        if let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() {
                            gauge.data_points().map(|point| (reason(point.attributes()), point.value())).collect()
                        } else if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                            sum.data_points().map(|point| (reason(point.attributes()), point.value())).collect()
                        } else {
                            panic!("{}: neither a gauge nor a counter", metric.name())
                        };
                    points.into_iter().map(|(reason, value)| (metric.name().to_string(), reason, value))
                })
                .collect();
            series.sort_unstable();
            series
        };
        let series = |name: &str, reason: &str, value: u64| (name.to_string(), reason.to_string(), value);

        MetricsEvent::from(ForgeMetrics::Enabled(false)).record_to_meter(&meter);
        assert_eq!(export(), [series("cardano_node_metrics_forging_enabled_int", "", 0)]);

        MetricsEvent::from(ForgeMetrics::Enabled(true)).record_to_meter(&meter);
        assert_eq!(
            export(),
            [
                series("cardano_node_metrics_Forge_forged_counter", "", 0),
                series("cardano_node_metrics_forging_enabled_int", "", 1),
                series("cardano_node_metrics_slotsMissed_int", "", 0),
            ]
        );

        for event in [
            ForgeMetrics::ScheduledSlots(3),
            ForgeMetrics::Forged,
            ForgeMetrics::MissedSlot { reason: "woke_late".to_string() },
            ForgeMetrics::MissedSlot { reason: "woke_late".to_string() },
            ForgeMetrics::MissedSlot { reason: "tip_ahead".to_string() },
        ] {
            MetricsEvent::from(event).record_to_meter(&meter);
        }
        assert_eq!(
            export(),
            [
                series("amaru_consensus_forge_missed_slot_total", "tip_ahead", 1),
                series("amaru_consensus_forge_missed_slot_total", "woke_late", 2),
                series("amaru_consensus_forge_scheduled_slots", "", 3),
                series("cardano_node_metrics_Forge_forged_counter", "", 1),
                series("cardano_node_metrics_forging_enabled_int", "", 1),
                series("cardano_node_metrics_slotsMissed_int", "", 3),
            ]
        );
    }
}
