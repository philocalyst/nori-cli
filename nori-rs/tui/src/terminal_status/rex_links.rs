//! Clicks on Nori's own links, routed back by Rex.
//!
//! A terminal does not give a program the clicks on links it printed. Rex reports them as
//! `url_clicked` events of the pane, which Nori follows through `nori_rex` (see
//! `docs/terminal-integration.md`). While that works, Nori draws its links
//! ([`crate::nori_link::set_routed`]); each click on one of this process's links becomes
//! [`AppEvent::OpenNoriLink`]. Outside Rex nothing starts and no links are drawn.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::nori_link::NoriLink;

/// Follow this pane's link clicks, when Nori runs in a Rex pane.
pub(crate) fn follow(app_event_tx: AppEventSender) {
    let Some(pane) = nori_rex::RexPane::from_env() else {
        return;
    };
    tokio::spawn(async move {
        let mut events = nori_rex::subscribe(pane);
        while let Some(event) = events.recv().await {
            match event {
                nori_rex::PaneEvent::Following => crate::nori_link::set_routed(true),
                nori_rex::PaneEvent::Lost { reason, retry_in } => {
                    tracing::debug!("Rex pane events lost ({reason}); retrying in {retry_in:?}");
                    crate::nori_link::set_routed(false);
                }
                nori_rex::PaneEvent::LinkClicked { url } => {
                    if NoriLink::parse(&url).is_some() {
                        app_event_tx.send(AppEvent::OpenNoriLink(url));
                    }
                }
            }
        }
        crate::nori_link::set_routed(false);
    });
}
