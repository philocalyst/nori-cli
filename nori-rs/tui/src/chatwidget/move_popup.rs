//! `/move`: choose the machine this session moves to.

use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;

impl ChatWidget {
    pub(crate) fn open_move_popup(&mut self) {
        let roster = nori_machines::global();
        if roster.is_empty() {
            self.add_info_message(
                "No other machines are set up.".to_owned(),
                Some(
                    "Add Rex hosts (`rex hosts add`) or `[machines.NAME] ssh = \"user@host\"` in config.toml."
                        .to_owned(),
                ),
            );
            return;
        }
        let items = roster
            .machines()
            .zip(roster.describe())
            .map(|(machine, line)| {
                let name = machine.name.to_string();
                let description = line
                    .strip_prefix(&format!("- {name}: "))
                    .unwrap_or(&line)
                    .to_owned();
                SelectionItem {
                    name: name.clone(),
                    description: Some(description),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::MoveSession {
                            machine: name.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        self.bottom_pane.show_selection_view(
            SelectionViewParams {
                title: Some("Move this session".to_owned()),
                subtitle: Some("History and settings move; credentials stay.".to_owned()),
                footer_hint: Some(standard_popup_hint_line()),
                items,
                header: Box::new(()),
                ..Default::default()
            }
            .picker(),
        );
    }
}
