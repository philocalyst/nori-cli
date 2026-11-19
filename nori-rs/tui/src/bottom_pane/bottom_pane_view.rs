use crate::bottom_pane::ApprovalRequest;
use crate::render::renderable::Renderable;
use crossterm::event::KeyEvent;
use nori_protocol::acp::v1 as acp;

use super::CancellationEvent;

/// Trait implemented by every view that can be shown in the bottom pane.
pub(crate) trait BottomPaneView: Renderable {
    /// Handle a key event while the view is active. A redraw is always
    /// scheduled after this call.
    fn handle_key_event(&mut self, _key_event: KeyEvent) {}

    /// Handle pointer input against the last rendered view geometry.
    fn handle_mouse_event(&mut self, _mouse: crossterm::event::MouseEvent) -> bool {
        false
    }

    /// Search pickers retain the draft bar in its usual bottom position.
    fn keeps_composer_visible(&self) -> bool {
        false
    }

    /// Return `true` if the view has finished and should be removed.
    fn is_complete(&self) -> bool {
        false
    }

    fn preserve_previous_views(&self) -> bool {
        false
    }

    /// Handle Ctrl-C while this view is active.
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        CancellationEvent::NotHandled
    }

    /// Handle Escape while this view is active. Views with multi-stage Escape
    /// behavior can override this independently from Ctrl-C cancellation.
    fn on_escape(&mut self) -> CancellationEvent {
        self.on_ctrl_c()
    }

    /// Optional paste handler. Return true if the view modified its state and
    /// needs a redraw.
    fn handle_paste(&mut self, _pasted: String) -> bool {
        false
    }

    /// Try to handle approval request; return the original value if not
    /// consumed.
    fn try_consume_approval_request(
        &mut self,
        request: ApprovalRequest,
    ) -> Option<ApprovalRequest> {
        Some(request)
    }

    /// A request this view may be asking about no longer needs an answer; drop it. Returns
    /// whether the view had it.
    fn withdraw_approval_request(&mut self, _request_id: &acp::RequestId) -> bool {
        false
    }
}
