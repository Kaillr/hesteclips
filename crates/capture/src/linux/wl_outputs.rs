//! The compositor's outputs (displays) in the order it announces them, which
//! is the order toolkits list them in (winit's `available_monitors()`), with
//! their names (`DP-1`): for putting a window fullscreen on a display. On
//! Wayland an app can't place its windows, only ask for one fullscreen on
//! an output.

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};

/// Each output, in the order announced: its name, once it's said (outputs
/// before version 4 of the protocol don't say).
struct State {
    names: Vec<Option<String>>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "wl_output"
        {
            let index = state.names.len();
            state.names.push(None);
            if version >= 4 {
                registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, index);
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event
            && let Some(n) = state.names.get_mut(*index)
        {
            *n = Some(name);
        }
    }
}

/// The outputs' names, in the compositor's order; none if it can't be asked.
pub(crate) fn names() -> Option<Vec<Option<String>>> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State { names: Vec::new() };
    // The globals, then what each output says of itself.
    queue.roundtrip(&mut state).ok()?;
    queue.roundtrip(&mut state).ok()?;
    Some(state.names)
}

#[cfg(test)]
mod tests {
    /// Each listed display is found among the compositor's outputs.
    #[test]
    #[ignore = "asks the desktop: cargo test -p capture display_outputs -- --ignored --nocapture"]
    fn display_outputs() {
        let names = super::names().expect("a Wayland compositor");
        eprintln!("outputs: {names:?}");
        for d in super::super::list_screens() {
            let at = super::super::display_monitor(&d.id);
            eprintln!("{} ({}) → {at:?}", d.name, d.id);
            assert!(at.is_some(), "{} not found", d.id);
        }
    }
}
