//! Where in the source an evaluation error arose.
//!
//! The AST carries no positions, but every property the parser read has a byte range. The
//! evaluator records which properties are being evaluated; when an error leaves one, the
//! chain of properties active at that moment (outermost first) is kept, so a host can show
//! `file:line:col` for the innermost property in its own module and name the modules the
//! error passed through.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use super::Evaluator;
use crate::error::Result;
use crate::parser::Property;
use crate::parser::PropertySource;

/// A property with the module text its ranges index into.
pub(super) struct PropertyLoc {
    property: Arc<Property>,
    module: Arc<str>,
    source: Arc<str>,
    declaration: Range<usize>,
    value: Option<Range<usize>>,
}

#[derive(Default)]
pub(super) struct SourceMap {
    properties: HashMap<usize, PropertyLoc>,
    active: Vec<usize>,
    /// The message of the error last seen leaving a property, and the properties active then.
    trace: Option<(String, Vec<usize>)>,
    /// Modules the error passed through, innermost first: what was being loaded and why.
    chain: Vec<String>,
    chain_message: Option<String>,
    /// What the last failure that left a member poisoned was about: its message, the
    /// property that declares what failed, and the object bodies being built at the time
    /// (innermost last), each as its properties.
    failure: Option<Failure>,
    /// The object bodies being built now, innermost last.
    bodies: Vec<Vec<Arc<Property>>>,
}

struct Failure {
    message: String,
    declared: Option<usize>,
    bodies: Vec<Vec<Arc<Property>>>,
}

/// An object being built when a member failed: the properties its body sets.
#[derive(Debug, Clone)]
pub struct ObjectFrame {
    pub properties: Vec<ErrorFrame>,
}

/// One property on the path to an evaluation error.
#[derive(Debug, Clone)]
pub struct ErrorFrame {
    /// The module the property is written in (a path or URI).
    pub module: String,
    pub property: String,
    /// The module's full text.
    pub source: Arc<str>,
    pub declaration: Range<usize>,
    pub value: Option<Range<usize>>,
}

impl SourceMap {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn register(&mut self, module: &str, source: &str, spans: Vec<PropertySource>) {
        if spans.is_empty() {
            return;
        }
        let module: Arc<str> = module.into();
        let source: Arc<str> = source.into();
        for span in spans {
            self.properties.insert(
                Arc::as_ptr(&span.property) as usize,
                PropertyLoc {
                    property: span.property,
                    module: Arc::clone(&module),
                    source: Arc::clone(&source),
                    declaration: span.declaration_span,
                    value: span.value_span,
                },
            );
        }
    }

    /// Note that `message` reports a failure of a member, with what declares it (when known)
    /// and the object bodies being built right now.
    pub(super) fn failed(&mut self, message: &str, declared: Option<&Property>) {
        let declared = declared
            .map(|property| property as *const Property as usize)
            .filter(|key| self.properties.contains_key(key));
        let keep = self.bodies.len().saturating_sub(6);
        self.failure = Some(Failure {
            message: message.to_owned(),
            declared,
            bodies: self.bodies[keep..].to_vec(),
        });
    }

    pub(super) fn enter_body(&mut self, entries: &[crate::parser::Entry]) {
        self.bodies.push(
            entries
                .iter()
                .filter_map(|entry| match entry {
                    crate::parser::Entry::Property(property) => Some(Arc::clone(property)),
                    _ => None,
                })
                .collect(),
        );
    }

    pub(super) fn leave_body(&mut self) {
        self.bodies.pop();
    }

    pub(super) fn enter(&mut self, property: &Property) -> bool {
        let key = property as *const Property as usize;
        let known = self.properties.contains_key(&key);
        if known {
            self.active.push(key);
        }
        known
    }

    pub(super) fn leave<T>(&mut self, result: &Result<T>) {
        if let Err(crate::Error::Eval(message)) = result
            && self.trace.as_ref().is_none_or(|(seen, _)| seen != message)
        {
            self.trace = Some((message.clone(), self.active.clone()));
            self.chain.clear();
        }
        self.active.pop();
    }

    /// Note that the error `message` passed out of the load of a module.
    pub(super) fn through_module(&mut self, message: &str, step: String) {
        if self.chain_message.as_deref() != Some(message) {
            self.chain.clear();
            self.chain_message = Some(message.to_owned());
        }
        self.chain.push(step);
    }
}

impl SourceMap {
    /// Where the properties of an object body were written. The evaluator works on copies of
    /// the parsed properties (it adds inherited types, for one), so each is matched to the
    /// parsed property of the same name and content; when the same text occurs more than once,
    /// the copies nearest the first property's match are taken, so the properties of one object
    /// literal resolve together.
    fn locate_body(&self, body: &[Arc<Property>]) -> Vec<ErrorFrame> {
        let candidates = |wanted: &Property| -> Vec<&PropertyLoc> {
            self.properties
                .values()
                .filter(|loc| {
                    loc.property.name == wanted.name
                        && loc.property.value == wanted.value
                        && loc.property.body == wanted.body
                })
                .collect()
        };
        let Some(first) = body.first().map(|property| candidates(property)) else {
            return Vec::new();
        };
        let spread = |anchor: &PropertyLoc| -> usize {
            body.iter()
                .map(|property| {
                    candidates(property)
                        .iter()
                        .filter(|loc| loc.module == anchor.module)
                        .map(|loc| loc.declaration.start.abs_diff(anchor.declaration.start))
                        .min()
                        .unwrap_or(usize::MAX / 8)
                })
                .sum()
        };
        let Some(anchor) = first.into_iter().min_by_key(|loc| spread(loc)) else {
            return Vec::new();
        };
        body.iter()
            .filter_map(|property| {
                candidates(property)
                    .into_iter()
                    .filter(|loc| loc.module == anchor.module)
                    .min_by_key(|loc| loc.declaration.start.abs_diff(anchor.declaration.start))
                    .map(Self::frame)
            })
            .collect()
    }

    fn frame(loc: &PropertyLoc) -> ErrorFrame {
        ErrorFrame {
            module: loc.module.to_string(),
            property: loc.property.name.to_string(),
            source: Arc::clone(&loc.source),
            declaration: loc.declaration.clone(),
            value: loc.value.clone(),
        }
    }
}

impl Evaluator {
    /// The property that declares what a member failure message reports, with the message.
    pub fn error_declaration(&self) -> Option<(String, ErrorFrame)> {
        let failure = self.spans.failure.as_ref()?;
        let loc = self.spans.properties.get(&failure.declared?)?;
        Some((failure.message.clone(), SourceMap::frame(loc)))
    }

    /// The object bodies that were being built when the member failure `message` arose,
    /// innermost first, each as the properties it sets. A failure in a member of the object
    /// is located by the property of that name in the innermost body that sets one.
    pub fn error_objects(&self, message: &str) -> Vec<ObjectFrame> {
        let Some(failure) = self.spans.failure.as_ref().filter(|f| f.message == message) else {
            return Vec::new();
        };
        failure
            .bodies
            .iter()
            .rev()
            .filter_map(|body| {
                let properties = self.spans.locate_body(body);
                (!properties.is_empty()).then_some(ObjectFrame { properties })
            })
            .collect()
    }

    /// Every property written with this name in any module read so far, in no fixed order.
    pub fn properties_named(&self, name: &str) -> Vec<ErrorFrame> {
        self.spans
            .properties
            .values()
            .filter(|loc| loc.property.name.as_ref() == name)
            .map(SourceMap::frame)
            .collect()
    }

    /// The properties that were being evaluated when the last evaluation error arose,
    /// outermost first. Empty when the error did not arise inside a property.
    pub fn error_frames(&self) -> Vec<ErrorFrame> {
        let Some((_, frames)) = &self.spans.trace else {
            return Vec::new();
        };
        frames
            .iter()
            .filter_map(|key| self.spans.properties.get(key))
            .map(SourceMap::frame)
            .collect()
    }

    /// The modules the last error passed through, innermost first, as
    /// `amended by main.pkl`-style steps.
    pub fn error_module_chain(&self) -> &[String] {
        &self.spans.chain
    }
}
