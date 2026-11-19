//! Describing a [`SelectionViewParams`] list as a [`ChoiceSpec`], so every list that is still
//! built as selection-view parameters (model and config pickers) shows on the Helix engine.

use super::ChoiceAction;
use super::ChoiceItem;
use super::ChoiceSpec;
use crate::bottom_pane::SelectionViewParams;

impl From<SelectionViewParams> for ChoiceSpec {
    fn from(params: SelectionViewParams) -> Self {
        let mut section: Option<String> = None;
        let mut items = Vec::with_capacity(params.items.len());
        // Header rows are not rows: they name the section of the rows after them. Indexes the
        // caller gave (`initial_selected_idx`) count the headers, so map them.
        let mut remap: Vec<Option<usize>> = Vec::new();
        for item in params.items {
            if item.is_header {
                section = Some(item.name);
                remap.push(None);
                continue;
            }
            remap.push(Some(items.len()));
            let disabled = item.actions.is_empty() && !item.is_current;
            let keep_open = !item.dismiss_on_select && !item.actions.is_empty();
            let actions = item.actions;
            items.push(ChoiceItem {
                label: item.name,
                detail: item.description,
                keywords: item.search_value,
                section: section.clone(),
                current: item.is_current,
                disabled,
                keep_open,
                keys: Vec::new(),
                action: ChoiceAction::Run(Box::new(move |tx| {
                    for action in &actions {
                        action(tx);
                    }
                })),
            });
        }
        let mut spec = ChoiceSpec::new(
            params
                .title
                .unwrap_or_else(|| "Select an option".to_owned()),
            items,
        );
        spec.subtitle = params.subtitle;
        spec.initial = params
            .initial_selected_idx
            .and_then(|index| remap.get(index).copied().flatten());
        spec.on_dismiss = params.on_dismiss;
        spec.on_secondary = params.on_shift_tab;
        spec
    }
}
