//! The general "choose one (or several) of these" picker: the Helix engine over a list the
//! caller describes as data ([`ChoiceSpec`]). Sections, search, disabled rows, multi-select,
//! questions (and chains of them), row keys, Shift-Tab, and lists that change while open come
//! with it; nothing here draws or filters.
//!
//! A spec is built where the choices are known (a config picker, the skillset list, the MCP
//! servers) and sent as `AppEvent::ShowChoicePicker`; the app shows it as an overlay. Choosing
//! runs an action; an action that leads somewhere answers with a [`Next`].

mod overlay;
mod selection;
mod spec;

pub(crate) use overlay::ChoiceOverlay;
#[cfg(feature = "storybook")]
pub use overlay::choice_specimen;
pub(crate) use spec::Ask;
pub(crate) use spec::ChoiceAction;
pub(crate) use spec::ChoiceItem;
pub(crate) use spec::ChoiceSpec;
pub(crate) use spec::Next;
pub(crate) use spec::NoticeAction;
pub(crate) use spec::StepAction;
