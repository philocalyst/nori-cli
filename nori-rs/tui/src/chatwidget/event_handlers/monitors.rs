//! The monitors the session's agent armed: how many watch, shown in the status line, and what they
//! reported to the agent, as one row each in the transcript.

use super::*;
use crate::history_cell::MonitorEventsCell;

impl ChatWidget {
    pub(in crate::chatwidget) fn on_monitor(&mut self, wire: nori_protocol::MonitorWire) {
        match wire {
            nori_protocol::MonitorWire::State(state) => {
                self.bottom_pane.set_monitor_count(state.watching);
            }
            nori_protocol::MonitorWire::Delivered(delivered) => {
                self.add_side_cell(MonitorEventsCell::new(delivered.monitors));
            }
        }
        self.request_redraw();
    }
}
