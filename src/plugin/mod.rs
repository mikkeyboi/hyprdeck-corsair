//! Hyprdeck plugin protocol: request dispatch, state rendering, actions, keyboard visualization,
//! key-tester input stream and companion service management. See docs/DESIGN.md §6.
//!
//! OWNER: plugin. Shared entry points used by main.rs.

mod actions;
mod backend;
mod labels;
mod model;
mod render;
mod service;
mod snapshot;
mod stream;
mod viz;

use anyhow::{Context, Result, bail};
use backend::{Backend, System};
use model::{Group, Row, State};
use render::{Daemon, View};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Internal deadline, leaving the host (15 s) time to read our explicit error.
const DEADLINE: Duration = Duration::from_secs(10);
/// Host stdout cap is 1 MiB; keep a margin for the envelope.
const MAX_RESPONSE: usize = 1024 * 1024 - 4096;
const MAX_ACTION_NAME: usize = 128;
pub const HOST_FEATURE_KEYBOARD: &str = "keyboard_visualization";

#[derive(Debug, Deserialize)]
struct Request {
    api_version: Value,
    method: Value,
    #[serde(default)]
    action: Value,
    #[serde(default = "empty_args")]
    args: Value,
    #[serde(default)]
    host_features: Value,
}

fn empty_args() -> Value {
    json!({})
}

/// A request that passed every check that needs no I/O.
#[derive(Debug, Clone, PartialEq)]
struct Validated {
    viz: bool,
    action: Option<actions::Parsed>,
}

fn validate(request: Request) -> Result<Validated> {
    if request.api_version.as_u64() != Some(1) {
        bail!(
            "Unsupported api_version {}; expected 1",
            request.api_version
        );
    }
    let Value::Object(args) = &request.args else {
        bail!("args must be a JSON object");
    };
    let viz = request.host_features.as_array().is_some_and(|features| {
        features
            .iter()
            .any(|f| f.as_str() == Some(HOST_FEATURE_KEYBOARD))
    });
    match request.method.as_str() {
        Some("state") => {
            if !request.action.is_null() {
                bail!("state requests must not include an action");
            }
            Ok(Validated { viz, action: None })
        }
        Some("action") => {
            let Some(name) = request.action.as_str() else {
                bail!("action requests need an action name");
            };
            if name.is_empty() || name.len() > MAX_ACTION_NAME || name.contains('\0') {
                bail!("Invalid action name");
            }
            Ok(Validated {
                viz,
                action: Some(actions::parse(name, args)?),
            })
        }
        _ => bail!(
            "Unknown method {}; expected state or action",
            request.method
        ),
    }
}

fn envelope_error(error: impl std::fmt::Display) -> Value {
    json!({"api_version": 1, "error": error.to_string(), "state": null})
}

/// Gather everything once, then read and render every keyboard (per-keyboard error isolation).
fn build_state(backend: &dyn Backend, viz: bool) -> State {
    let now = backend.now();
    let daemon = match backend.daemon_hello() {
        Ok(None) => Daemon::NotRunning,
        Ok(Some(devices)) => Daemon::Running(devices),
        Err(error) => Daemon::Error(format!("{error:#}")),
    };
    let service = service::status(backend, daemon.running());
    let input_rule = backend.input_rule_installed();
    let live = crate::config::load_live().map_err(|error| format!("{error:#}"));
    let looks = crate::config::load_looks().map_err(|error| format!("{error:#}"));
    let mut groups = Vec::new();
    let mut products = Vec::new();
    let keyboards = backend.keyboards();
    for keyboard in &keyboards {
        let via_daemon = snapshot::owned_by_daemon(daemon.devices(), keyboard);
        let snapshot = snapshot::read(backend, keyboard, via_daemon, now);
        let view = View {
            viz,
            daemon: &daemon,
            service,
            input_rule,
            live: &live,
            looks: &looks,
            writes_today: crate::storage::WriteBudget::used_today(&keyboard.id),
        };
        let (device_groups, product) = render::keyboard(&view, keyboard, &snapshot);
        groups.extend(device_groups);
        products.push(product);
    }
    if keyboards.is_empty() {
        let mut group = Group::new(
            "no-keyboards",
            "No supported keyboard found",
            "Supports the Corsair K65 PLUS Wireless (receiver or cable) and K100 RGB",
        );
        group.row(Row::info(
            "hint",
            "Connect a keyboard",
            "Plug in the keyboard or its SLIPSTREAM receiver, then refresh. If it is connected, \
             install the 70-hyprdeck-corsair.rules udev rule and replug it.",
        ));
        for (pid, name) in backend.unsupported() {
            group.row(Row::info(
                format!("unsupported-{pid:04x}"),
                name,
                format!("USB 1b1c:{pid:04x}"),
            ));
        }
        group.row(Row::info("companion", "Background helper", service.label()));
        groups.push(group);
    }
    State {
        title: "Corsair keyboards".into(),
        description:
            "Onboard lighting, keys and power for Corsair keyboards. Settings change only when \
                      you apply them."
                .into(),
        groups,
        products,
        refresh_interval_ms: 2000,
    }
}

fn dispatch(backend: &dyn Backend, validated: &Validated) -> Result<State> {
    if let Some(parsed) = &validated.action {
        actions::execute(backend, parsed, backend.now())?;
    }
    Ok(build_state(backend, validated.viz))
}

/// Serialize the success envelope, dropping visualizations if the state would exceed the cap.
fn success(mut state: State) -> Value {
    let envelope = |state: &State| json!({"api_version": 1, "error": null, "state": state});
    let value = envelope(&state);
    if value.to_string().len() <= MAX_RESPONSE {
        return value;
    }
    for group in &mut state.groups {
        group.visualization = None;
    }
    let value = envelope(&state);
    if value.to_string().len() <= MAX_RESPONSE {
        return value;
    }
    envelope_error("The keyboard state is too large to display — remove some remaps or macros")
}

fn run_validated(validated: Validated) -> Value {
    let started = Instant::now();
    let budget = crate::bragi::RequestBudget {
        work: started + DEADLINE,
        cleanup: started + Duration::from_secs(13),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    // Do not detach: every mode/resource guard must finish before the CLI can exit.
    // Transport/resource loops enforce the work deadline and restoration uses the cleanup reserve.
    let cancel = budget.cancelled.clone();
    let spawned = std::thread::Builder::new()
        .name("hyprdeck-corsair-request".into())
        .spawn(move || {
            crate::bragi::with_request_budget(budget, || {
                dispatch(&System, &validated).map_err(|error| format!("{error:#}"))
            })
        });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(error) => return envelope_error(format!("Could not start the request: {error}")),
    };
    while !thread.is_finished() && Instant::now() < started + DEADLINE {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !thread.is_finished() {
        cancel.store(true, Ordering::Release);
    }
    let result = thread.join();
    if Instant::now() >= started + DEADLINE {
        let detail = match result {
            Ok(Err(error)) => format!("; {error}"),
            Err(_) => "; worker panicked during cleanup".into(),
            _ => String::new(),
        };
        return envelope_error(format!(
            "Request deadline expired; work cancelled and joined after cleanup{detail}"
        ));
    }
    match result {
        Ok(Ok(state)) => success(state),
        Ok(Err(error)) => envelope_error(error),
        Err(_) => envelope_error("Internal error while handling the request; cleanup attempted"),
    }
}

/// Handle one `--request` stdin payload (already size-capped) and return the response envelope
/// `{"api_version":1,"error":…,"state":…}`. Never panics on bad input; enforces the 13 s deadline.
pub fn run_request(input: &[u8]) -> Value {
    let request: Request = match serde_json::from_slice(input) {
        Ok(request) => request,
        Err(error) => return envelope_error(format!("Invalid request JSON: {error}")),
    };
    match validate(request) {
        Ok(validated) => run_validated(validated),
        Err(error) => envelope_error(format!("{error:#}")),
    }
}

/// Human CLI: `state` or `action <name> [json-args]`; returns the envelope (pretty-printed by main).
pub fn run_cli(method: &str, action: Option<&str>, args: Option<&str>) -> Value {
    let args = match args.map(serde_json::from_str::<Value>).transpose() {
        Ok(args) => args.unwrap_or_else(empty_args),
        Err(error) => return envelope_error(format!("Invalid JSON args: {error}")),
    };
    let request = Request {
        api_version: json!(1),
        method: json!(method),
        action: action.map_or(Value::Null, |name| json!(name)),
        args,
        host_features: Value::Null,
    };
    match validate(request) {
        Ok(validated) => run_validated(validated),
        Err(error) => envelope_error(format!("{error:#}")),
    }
}

/// `--input-stream <device id>`: NDJSON key-tester frames to stdout until killed or stdout closes.
pub fn run_input_stream(device_id: &str) -> anyhow::Result<()> {
    if !device_id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        || device_id.is_empty()
    {
        bail!("Invalid keyboard id");
    }
    stream::run(device_id).context("Key tester")
}

#[cfg(test)]
mod tests;
