use anyhow::Result;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::info;

pub fn init_metrics(port: u16) -> Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(([0, 0, 0, 0], port))
        .install()
        .map_err(|e| anyhow::anyhow!("prometheus install: {e}"))?;

    info!(port = port, "prometheus metrics server started");
    Ok(())
}

pub fn record_request_sent(request_type: &str) {
    counter!("sn2_requests_sent_total", "type" => request_type.to_string()).increment(1);
}

pub fn record_response(success: bool, response_time: f64) {
    let status = if success { "success" } else { "failure" };
    counter!("sn2_responses_total", "status" => status.to_string()).increment(1);
    histogram!("sn2_response_time_seconds").record(response_time);
}

pub fn set_active_tasks(count: usize) {
    gauge!("sn2_active_tasks").set(count as f64);
}

pub fn set_dispatch_pressure(scale: f64, memory_available_ratio: Option<f64>) {
    gauge!("sn2_dispatch_pressure_scale").set(scale);
    if let Some(ratio) = memory_available_ratio {
        gauge!("sn2_host_memory_available_ratio").set(ratio);
    }
}

pub fn record_dispatch_pressure_change(direction: &str) {
    counter!(
        "sn2_dispatch_pressure_changes_total",
        "direction" => direction.to_string()
    )
    .increment(1);
}

pub fn record_dispatch_allocation(
    fair_slots: usize,
    residual_slots: usize,
    starved_miners: usize,
    utilization: f64,
) {
    counter!("sn2_dispatch_fair_slots_total").increment(fair_slots as u64);
    counter!("sn2_dispatch_residual_slots_total").increment(residual_slots as u64);
    counter!("sn2_dispatch_starved_miners_total").increment(starved_miners as u64);
    histogram!("sn2_dispatch_slot_utilization_ratio").record(utilization.clamp(0.0, 1.0));
}

pub fn set_metagraph_n(n: u16) {
    gauge!("sn2_metagraph_n").set(n as f64);
}

pub fn record_weight_update() {
    counter!("sn2_weight_updates_total").increment(1);
}
