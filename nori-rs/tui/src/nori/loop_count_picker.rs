//! The loop count choices: a few presets and a "Custom…" row that asks for a number.

use std::sync::Arc;

use crate::app_event::AppEvent;
use crate::choice_picker::Ask;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;
use crate::choice_picker::Next;

/// Maximum allowed custom loop count.
const MAX_LOOP_COUNT: i32 = 1000;

/// Preset options (`None` turns looping off).
const PRESETS: [Option<i32>; 5] = [None, Some(2), Some(3), Some(5), Some(10)];

/// The picker for the loop count, with `current` marked.
pub(crate) fn loop_count_choices(current: Option<i32>) -> ChoiceSpec {
    let mut items: Vec<ChoiceItem> = PRESETS
        .iter()
        .map(|&preset| {
            let label = match preset {
                Some(count) => count.to_string(),
                None => "Disabled".to_owned(),
            };
            ChoiceItem::run(
                label,
                Box::new(move |tx| tx.send(AppEvent::SetConfigLoopCount(preset))),
            )
            .current(preset == current)
        })
        .collect();
    let custom_current = (!PRESETS.contains(&current)).then_some(current).flatten();
    items.push(ChoiceItem {
        current: custom_current.is_some(),
        detail: Some(format!("1 to {MAX_LOOP_COUNT}")),
        ..ChoiceItem::ask(
            match custom_current {
                Some(count) => format!("Custom… ({count})"),
                None => "Custom…".to_owned(),
            },
            Ask {
                verb: "iterations",
                label: String::new(),
                initial: String::new(),
                secret: false,
                then: Arc::new(|text, tx| {
                    let count: i32 = text
                        .trim()
                        .parse()
                        .map_err(|_| format!("`{}` is not a number", text.trim()))?;
                    // One iteration is no loop.
                    let value = (count > 1).then_some(count.min(MAX_LOOP_COUNT));
                    tx.send(AppEvent::SetConfigLoopCount(value));
                    Ok(Next::Done)
                }),
            },
        )
    });
    let mut spec = ChoiceSpec::new("Loop count", items);
    spec.subtitle = Some("How many times a prompt repeats".to_owned());
    spec
}
