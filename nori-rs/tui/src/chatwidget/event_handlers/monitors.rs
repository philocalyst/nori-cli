//! The monitors the session's agent armed and the jobs it follows: how many there are, shown in the
//! status line, and what the monitors reported to the agent, as one row each in the transcript.

use super::*;
use crate::history_cell::MonitorEventsCell;

impl ChatWidget {
    pub(in crate::chatwidget) fn on_monitor(&mut self, wire: nori_protocol::MonitorWire) {
        match wire {
            nori_protocol::MonitorWire::State(state) => {
                self.bottom_pane
                    .set_background(crate::bottom_pane::Background {
                        monitors: state.watching,
                        jobs: state.jobs,
                    });
            }
            nori_protocol::MonitorWire::Delivered(delivered) => {
                self.add_side_cell(MonitorEventsCell::new(delivered.monitors));
            }
        }
        self.request_redraw();
    }
}
