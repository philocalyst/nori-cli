//! The one-line prompt of search (`/`, `?`) and of selecting by pattern (`s`, `S`, `K`).

use modalkit::actions::Action;
use modalkit::actions::CommandBarAction;
use modalkit::actions::Editable;
use modalkit::actions::EditorAction;
use modalkit::editing::buffer::CursorGroupId;
use modalkit::editing::buffer::EditBuffer;
use modalkit::editing::context::EditContext;
use modalkit::editing::cursor::Cursor;
use modalkit::editing::store::Store;
use modalkit::prelude::CommandType;
use modalkit::prelude::ViewportContext;

use crate::editor::Editor;
use crate::editor::EditorEvent;
use crate::info::EditorInfo;
use crate::text::Text;

pub(crate) struct Prompt {
    /// What the prompt asks for (`search:`).
    pub(crate) label: String,
    kind: CommandType,
    /// What runs on Enter, with the typed pattern as the last search.
    then: Action<EditorInfo>,
    buffer: EditBuffer<EditorInfo>,
    gid: CursorGroupId,
    view: ViewportContext<Cursor>,
    /// The typed line and the cursor's byte offset in it, as of the last key.
    pub(crate) line: String,
    pub(crate) cursor: usize,
}

impl Prompt {
    fn new(label: String, kind: CommandType, then: Action<EditorInfo>) -> Self {
        let mut buffer = EditBuffer::new("*nori-prompt*".to_owned());
        let gid = buffer.create_group();
        Prompt {
            label,
            kind,
            then,
            buffer,
            gid,
            view: ViewportContext::default(),
            line: String::new(),
            cursor: 0,
        }
    }

    pub(crate) fn editor_action(
        &mut self,
        action: &EditorAction,
        ctx: &EditContext,
        store: &mut Store<EditorInfo>,
    ) {
        let _ = self
            .buffer
            .editor_command(action, &(self.gid, &self.view, ctx), store);
        let text = Text::of_rope(self.buffer.get());
        let cursor = text.offset(&self.buffer.get_leader(self.gid));
        self.line = text.text.lines().next().unwrap_or("").to_owned();
        self.cursor = cursor.min(self.line.len());
    }
}

impl Editor {
    pub(crate) fn command_bar(&mut self, action: CommandBarAction<EditorInfo>) {
        match action {
            CommandBarAction::Focus(label, kind, then) => {
                self.prompt = Some(Prompt::new(label, kind, *then));
            }
            CommandBarAction::Unfocus => self.prompt = None,
        }
    }

    pub(crate) fn submit_prompt(&mut self, ctx: &EditContext) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let pattern = prompt.line;
        if pattern.is_empty() {
            return;
        }
        if let Err(error) = regex::Regex::new(&pattern) {
            self.events
                .push(EditorEvent::Notice(format!("invalid pattern: {error}")));
            return;
        }
        self.store
            .registers
            .set_last_command(prompt.kind, pattern.as_str());
        self.apply(prompt.then, ctx.clone());
    }

    /// The prompt's label, text and cursor, while it is open.
    pub fn prompt_line(&self) -> Option<(&str, &str, usize)> {
        let prompt = self.prompt.as_ref()?;
        Some((&prompt.label, &prompt.line, prompt.cursor))
    }
}
