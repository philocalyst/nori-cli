//! Fallbacks, as `/providers` shows them: the order a session moves through accounts and models
//! when a request fails, read from the user's Pkl policy (`$NORI_HOME/init.pkl` over Nori's own
//! `default.pkl`). Pkl stays the one place fallbacks are written; this page only reads it, and
//! "Edit in init.pkl" points at the file, creating it with a starting block when there is none.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::app_event::AppEvent;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;
use crate::choice_picker::Next;
use crate::choice_picker::StepAction;

/// The fallbacks page for a Nori home; Esc goes `back`.
pub(crate) fn spec(nori_home: &Path, back: StepAction) -> ChoiceSpec {
    let path = nori_pkl::global_policy_path(nori_home);
    let mut items = Vec::new();
    let subtitle = match nori_pkl::load_policy(Some(&path), None) {
        Ok(workflow) => {
            let fallback = workflow.recovery.base.fallback;
            if fallback.chain.is_empty() {
                items.push(
                    info("No routes of your own", "nothing is listed in chain").section("In order"),
                );
            }
            for (position, route) in fallback.chain.iter().enumerate() {
                let agent = route.agent.as_deref().map_or_else(
                    || "the agent in use".to_owned(),
                    |slug| {
                        nori_acp_host::accounts::find(slug)
                            .map_or_else(|| slug.to_owned(), |account| account.display_name)
                    },
                );
                let model = route.model.as_deref().unwrap_or("its default model");
                items.push(
                    info(format!("{}. {agent}", position + 1), model.to_owned())
                        .section("In order"),
                );
            }
            items.push(
                info(
                    "Every other installed route",
                    if fallback.ranked {
                        "ranked, the model's own provider first"
                    } else {
                        "off: only the routes above"
                    },
                )
                .section("Then"),
            );
            items.push(
                info(
                    "Accounts with room first",
                    if fallback.prefer_headroom {
                        "of two accounts for one model, the one with more left; a spent one last"
                    } else {
                        "off: accounts keep the order above"
                    },
                )
                .section("Then"),
            );
            let back_to = match fallback.return_to_primary.name() {
                "never" => "stays on the fallback for the rest of the conversation",
                "after_reset" => "once the failure is over (a quota window resets)",
                _ => "the next session starts on your route again",
            };
            items.push(info("Back to your route", back_to).section("Then"));
            let kinds: Vec<&str> = fallback.on.iter().map(|kind| kind.label()).collect();
            format!("when a request fails with {}", kinds.join(", "))
        }
        Err(error) => {
            items.push(info("init.pkl does not load", format!("{error}")).section("In order"));
            "Nori uses its own defaults until it loads".to_owned()
        }
    };
    items.push(edit_item(path).section("Pkl"));
    // Everything else only says something: start on the one row that does.
    let edit = items.len() - 1;
    let mut page = ChoiceSpec::new("Fallbacks", items);
    page.initial = Some(edit);
    page.subtitle = Some(subtitle);
    page.on_back = Some(back);
    page
}

/// A line of the page that says something and does nothing.
fn info(label: impl Into<String>, detail: impl Into<String>) -> ChoiceItem {
    let mut item = ChoiceItem::run(label, Box::new(|_| {})).detail(detail);
    item.disabled = true;
    item
}

/// "Edit in init.pkl": say where the file is, creating it with a fallback block first when the
/// user has none yet.
fn edit_item(path: PathBuf) -> ChoiceItem {
    let exists = path.exists();
    let shown = path.display().to_string();
    ChoiceItem::run(
        "Edit in init.pkl",
        Box::new(move |tx| {
            let created = !path.exists() && std::fs::write(&path, starter()).is_ok();
            let hint = if created {
                "created with an empty chain: list routes in it, Nori reads it at the next fallback"
            } else {
                "fallback.chain lists routes in order; Nori reads it at the next fallback"
            };
            tx.send(AppEvent::InsertHistoryCell(Box::new(
                crate::history_cell::new_info_event(
                    format!("Fallbacks are Pkl, in {}", path.display()),
                    Some(hint.to_owned()),
                ),
            )));
        }),
    )
    .detail(if exists {
        shown
    } else {
        format!("{shown} · created when chosen")
    })
}

/// A new `init.pkl`: the fallback block, with every account Nori knows offered as a route.
fn starter() -> String {
    let accounts: Vec<String> = nori_acp_host::accounts::list()
        .into_iter()
        .map(|account| {
            format!(
                "      // new Dynamic {{ agent = \"{}\"; model = \"…\" }}, // {}",
                account.slug, account.display_name
            )
        })
        .collect();
    let accounts = accounts.join("\n");
    format!(
        "// Your Nori policy. It layers over Nori's own default.pkl: state only what you change.\n\
         recovery = new Dynamic {{\n  \
           fallback = new Dynamic {{\n    \
             // Routes to move to, in order, when a request fails with quota_exhausted,\n    \
             // model_unavailable or capacity. `agent` is an account's agent.\n    \
             chain = List(\n{accounts}\n    )\n  \
           }}\n\
         }}\n"
    )
}

/// The row of `/providers` that opens this page.
pub(crate) fn entry(nori_home: Arc<PathBuf>, back: StepAction) -> ChoiceItem {
    ChoiceItem::step(
        "Fallback order",
        Arc::new(move |_| Next::Show(spec(&nori_home, back.clone()))),
    )
    .detail("where a session goes when a request fails · Pkl")
}
