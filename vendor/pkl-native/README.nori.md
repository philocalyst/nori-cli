# Native Pkl engine embedded by Nori

This is the existing Nori native Pkl spike, copied from
`personal/pkl-rust/engine` into tracked source for production integration.
Its foundation is MIT-licensed jdx/pklr 6.0.3 at
`f20515796cd4986c4f3dcba929fd6ddfdaedc1c1`; `LICENSE` and `PROVENANCE.json`
retain the original attribution. `NORI-SPIKE.md` preserves the spike's measured
conformance results and known gaps. Its references to an isolated, nonproduction
experiment describe the source before this integration.

The crate package is renamed `nori-pkl-runtime`; its Rust library remains `pklr`.
Nori calls it directly in-process. No evaluator-server bridge, Pkl executable,
or Java runtime participates in evaluation.

Integration changes add `Evaluator::eval_source_data` for typed host data,
preserving explicit null policy overrides, rejecting deferred member failures,
and retaining the recorded module's original import base. Engine trace and
deprecation messages use `tracing` so they cannot write into the live TUI.
The parser also exposes opt-in `parse_named_spanned` metadata, retaining syntax
node identities and exact property byte ranges. Nori uses this same parser for
lossless structural edits and literal journal inspection. Ordinary evaluation
does not allocate the span table; journal replay never invokes the evaluator.

This engine is still an incomplete implementation of Pkl. The preserved spike
report identifies language, standard library, rendering, and diagnostic gaps.
Nori does not fall back to an external evaluator for those cases.
