//! Turning the catalog's gates into matchers.

use nori_pkl::catalog::Catalog;
use nori_pkl::kinds::GateDef;
use nori_pkl::kinds::gate::Rule;

use super::Entry;
use super::Gates;
use super::host_tool::HostTool;
use super::permission::Permission;
use super::stop::Stop;

impl Gates {
    /// Compiles every gate of `catalog`. A gate that cannot compile is left out whole (half a gate
    /// would decide differently from what its author wrote) and reported in the second value, with
    /// its file; validation already caught what a module can get wrong, so this is for the limits
    /// of the regular expression engine.
    pub(crate) fn compile(catalog: &Catalog) -> (Self, Vec<String>) {
        let mut gates = Self::default();
        let mut problems = Vec::new();
        for loaded in &catalog.gates {
            let mut compiled = Self::default();
            match compiled.add(&loaded.value) {
                Ok(()) => {
                    gates.permission.append(&mut compiled.permission);
                    gates.host_tool.append(&mut compiled.host_tool);
                    gates.stop.append(&mut compiled.stop);
                }
                Err(message) => problems.push(format!(
                    "{}: gate `{}` is not in force: {message}",
                    loaded.path.display(),
                    loaded.name
                )),
            }
        }
        (gates, problems)
    }

    fn add(&mut self, def: &GateDef) -> Result<(), String> {
        for (position, rule) in def.rules.iter().enumerate() {
            let index = i64::try_from(position).unwrap_or(i64::MAX);
            let at = format!("rules[{index}]");
            let gate = def.name.clone();
            match rule {
                Rule::Permission { .. } => self.permission.push(Entry {
                    gate,
                    index,
                    rule: Permission::compile(&at, rule)?,
                }),
                Rule::HostTool { .. } => self.host_tool.push(Entry {
                    gate,
                    index,
                    rule: HostTool::compile(&at, rule)?,
                }),
                Rule::Stop { .. } => self.stop.push(Entry {
                    gate,
                    index,
                    rule: Stop::compile(&at, rule)?,
                }),
            }
        }
        Ok(())
    }
}
