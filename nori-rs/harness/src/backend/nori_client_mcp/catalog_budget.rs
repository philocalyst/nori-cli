//! The always-loaded tool catalog is paid for on every turn of every session, so its size is
//! pinned: growing it past the budget is a decision, not an accident.

use super::NoriClientService;
use super::tool_instructions;

/// Characters per token, roughly.
const CHARS_PER_TOKEN: usize = 4;
/// What the default catalog (tool descriptions and schemas) plus the instructions may cost.
/// Measured at 12.7k when this was written; the file and LSP tools are not in it.
const BUDGET_TOKENS: usize = 13_500;

#[test]
fn default_catalog_stays_within_its_token_budget() {
    let mut tools = NoriClientService::all_tools();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut total = 0;
    for tool in &tools {
        let description = tool.description.as_deref().unwrap_or_default().len();
        let schema = serde_json::to_string(&*tool.input_schema)
            .map(|schema| schema.len())
            .unwrap_or_default();
        total += description + schema;
        eprintln!(
            "catalog {:<22} description {:>5} schema {:>5} tokens {:>5}",
            tool.name,
            description,
            schema,
            (description + schema) / CHARS_PER_TOKEN
        );
    }
    let instructions = tool_instructions().len();
    eprintln!(
        "catalog tools {} total tokens {} instructions tokens {}",
        tools.len(),
        total / CHARS_PER_TOKEN,
        instructions / CHARS_PER_TOKEN
    );
    assert!(
        (total + instructions) / CHARS_PER_TOKEN <= BUDGET_TOKENS,
        "the default tool catalog outgrew its budget"
    );
}
