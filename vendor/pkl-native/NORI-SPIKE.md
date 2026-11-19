# Experimental native Rust Pkl implementation

This work is **unfinished**. It is isolated from Nori's production `nori-pkl`
crate, which uses the official evaluator through `pklrust`. Do not replace the
production evaluator with this implementation on the strength of these tests.

The native foundation is the MIT-licensed [`jdx/pklr`](https://github.com/jdx/pklr)
6.0.3 implementation, checked out at
`f20515796cd4986c4f3dcba929fd6ddfdaedc1c1` in `engine/`. It has its own Rust lexer,
parser, evaluator, runtime, imports, type checking, and renderers. The changes
here extend that implementation; they are not a claim to have authored its
existing implementation from scratch. `engine/LICENSE` retains its license.

The official oracle and checked-in goldens come from
[`apple/pkl` 0.33.0](https://github.com/apple/pkl/tree/0.33.0), revision
`5ee1f296e3503abd8ac8e9b27d197b2f779ea408`, in `upstream/`. `PROVENANCE.json`
records the source pins and verified official executable SHA-256 digest.

## Native changes

- Module values retain their class through locals, collections, closures, and
  `outer`. Capturing a module waits for sibling values, while explicit field
  reads retain their dependencies. Hidden inherited members remain readable
  through imports and stay out of rendered maps. Module amendments retain their
  evaluated public members even when hidden metadata is present.
- Java regex quotation is normalized before compiling patterns or generating
  anchors. Quoted spaces, comment markers, newlines, and unterminated quotations
  remain literal. Extended mode ignores exactly Java's six ASCII whitespace
  characters, including inside character classes; original pattern text and
  regex identity remain intact.
- Maps retain their class and original key values through class lookup, text,
  `containsKey`, `filter`, and `mapValues`; Map equality stays distinct from
  Mapping and Dynamic. Function values under a `toString` key remain values.
- Imported modules retain class identity through amendments and extensions,
  including their ancestor classes and module snapshots captured by functions.
  Default-marked type imports and transitive aliases resolve in their declaring
  scope, including when a consumer shadows an import as a value.
- Parsed JSON/YAML Dynamic members retain property identity through simple
  amendments, so renderers and class converters distinguish them from entries.
  YAML `parseAll` returns a List of documents; sequences inside each document
  remain Listings.
- Typed-object value text and caught diagnostics use definition-site class names.
  Declared module names survive construction, amendment, and `getClass`, while
  canonical identities keep separately imported classes distinct. Pair text uses
  its constructor form, and standard module URI names display as `pkl.json`.
- `Null(default)` retains a typed object, Dynamic, Listing, Mapping, or function
  amendment parent. Object literals remain lazy; null equality, collection
  keys, null-safe access, renderers, and Jsonnet's nullable indent keep seeing null.
  Parameterized amendments preserve Pkl's distinction between ordinary Dynamic
  bodies and generator bodies, and check function arity at construction.
- Nullable scalar, collection, and Mixin defaults retain their amendment parent;
  Mixin's type default is the identity function. Nullable typed-class expression
  defaults still need a deferred prototype representation and remain incomplete.
- `pkl:test.catch` and `catchOrNull` force deferred collection failures, matching
  the official test library rather than treating an unread failed element as success.
- Member predicates can amend Listing and Mapping members, with predicate/index
  conflicts rejected and focused regression tests.
- Concrete classes must satisfy inherited abstract methods within a module;
  abstract methods in concrete classes/modules are rejected. Imported parent
  obligations remain incomplete.
- `eval_fixture` evaluates with the environment and properties used by Apple's
  snippet harness.
- Test directories use canonical paths on macOS, unique sequence numbers avoid
  concurrent fixture collisions, and glob fixtures avoid case-only filenames.
- `protocol/` implements experimental streaming evaluator-server framing and
  PklBinary encoding for JSON's representable subset. It deliberately rejects
  unsupported evaluator options and expression requests. Typed identity and
  non-JSON values are not preserved by that bridge.

## Nori integration changes

- `Evaluator::eval_source_data` renders a `Duration` or `DataSize` as
  `{"value": 500, "unit": "ms"}` (the JSON renderer still rejects them, as Pkl does).
- Amending a property a class declared `Retry?` (or any nullable class type) instantiates the
  class the declaring module bound, not whatever the type name means in the amending module.
  The three baseline recursive-Listing scenarios (`imports::typed_listing_elements_of_recursive_classes_in_an_imported_module`,
  `objects::nested_listing_fallback_only_completes_unresolved_class_names`,
  `objects::nested_listings_of_a_class_naming_itself_build_their_innermost_elements`) were
  re-run through Nori's evaluator and produce the values their tests expect; the vendor
  suite itself was not re-run after this change.
- `new Unknown { ... }` fails with ``Cannot find type `Unknown`.`` instead of building an
  untyped object.
- Nori's embedding (`nori-pkl` `native/diagnostics.rs`) adds `file:line` and the source line to
  syntax errors, and locates evaluation errors in the module's own source; the engine itself
  still reports no spans for evaluation errors.
- Fixed at its root: an amending module that imports a schema module under the same alias the
  parent schema uses (`import "nori:Tool" as Tool`) lost the schema's class defaults. Two causes:
  a module imported in full by one module and narrowed to its used fields by another produced
  different class instances (aliases a base module reads are no longer narrowed, and embedded or
  remote modules are always evaluated in full, once per evaluation).
- Constraints on a plain value an amending module assigns to a constrained inherited property
  (`name = ""` against `name: String(!isEmpty)`) are enforced; they were skipped.
- `amends`/`extends` of a file that does not exist, and a used import of one, fail with
  `Import not found` instead of evaluating an empty module.
- Spans: every property the parser reads has a recorded byte range. When a member fails (type,
  constraint, undefined property), the evaluator records the object bodies being built at that
  moment and `Evaluator::error_objects(message)` returns them innermost first, each with the
  written location of every property it sets (matched by name and content, nearest the object's
  first property), so a host points at the failing object literal's own member rather than
  searching the file. `error_frames`, `error_declaration` and `properties_named` expose the
  property being evaluated, the declaring property and the table; `error_module_chain` names the
  modules (imports, `amends`, `extends`) an error passed through. Evaluation errors still carry
  no span in their own value.
- The vendor suite was not re-run after these changes.

## Run the conformance gates

The source archive omits the large oracle binary and upstream checkout; run
`./bootstrap.sh` after unpacking to restore the pinned sources and checksum-verified
official executable. The existing workspace already contains both.

The workspace preserves validated standalone evaluators in `bin/eval_fixture`
and `bin/eval_json`, with release copies under `engine/target/release/examples`
for the gates. Continued development can reuse `.target-conformance-fast` through
`CARGO_TARGET_DIR`, with `CARGO_BUILD_JOBS=2` and development/test debug information
disabled. On this host, use the exact compiler and local sysroot wrapper recorded
in `reports/regex-local-sysroot.json`. The archive excludes compiler caches and
evaluator binaries.

```sh
cd engine
cargo build --release --example eval_fixture
cd ..
python3 run-conformance.py
python3 run-goldens.py
```

Both gates **exit nonzero while gaps remain**. They select all 1,074 `.pkl`
fixtures beneath Apple's `LanguageSnippetTests/input` directory. The first
compares JSON outputs, falling back to exact text when JSON cannot represent a
value. Matching rejection status is recorded separately and does not establish
diagnostic equivalence. Infrastructure skips remain unresolved.

The second compares native text and diagnostics with Apple's checked-in golden
files, using Apple's path/line normalization rules. An omitted golden means an
expected empty successful output, matching `SnippetOutcome`. This stricter gate
checks ordering, class names, rendering, and diagnostic text that JSON
comparison cannot establish. Binary golden debug-rendering is not implemented.

Latest measured results are in `reports/latest.json` and `reports/goldens.json`.
Each records the exact executable hash; source can continue to change after an
artifact is built, so rebuild before evaluating subsequent changes. No score in
these reports establishes full Pkl compatibility.

`adversarial/run.py` adds fixtures outside the upstream suite. The current matrix
has 75 fixtures: 62 identical successful JSON outputs, 11 shared errors, and two
native missed errors (accepting anonymous `new { ... }` without an inferable
parent). Shared errors are not scored as semantic parity.

## Validation and remaining gaps

A full engine run passed 1,845 tests, with seven existing ignored tests and
three documented baseline failures excluded. All eighteen module capture
regressions passed. Their eighteen source fixtures matched the official 0.33
evaluator exactly using the final optimized executable (three matched before
this change); all twenty-three prior Map and regex oracle fixtures also matched.
The final source passed Clippy for the library and tests with `-D warnings`;
formatting ran last with no tests rerun afterward. See
`reports/module-capture-oracle-probes.json` and `reports/validation.json`.

Earlier null/default tests (12), abstract-method tests (4), predicate tests (13),
Map/module tests (12), and resource tests (9) remain covered by the engine suite.
The protocol's previous validation passed two unit tests and three real-process tests through the official
`pklrust` binding; it was not rebuilt or re-tested for this module capture
compatibility change.
The three existing recursive Listing/type-default failures reproduce in the
pristine foundation checkout in `baseline-engine/`; exact names and logs remain
in `reports/known-baseline-failures.json`. Failed attempts and final validation
logs are preserved under `reports/`.

The latest full 1,074-fixture run has 401 exact golden output matches and 436
semantic output matches; 161 differential statuses remain unresolved. Module
capture adds `types/currentModuleType5.pkl` and
`types/currentModuleTypeInParentMembers.pkl` to both sets, with no previous output
matches lost in either complete gate. The two remaining timeout fixtures are
unchanged. Shared rejection statuses are not proof of diagnostic equivalence.
Evidence is in `reports/module-capture-progress.json`; earlier rounds remain in
`reports/regex-progress.json`, `reports/map-progress.json`,
`reports/render-progress.json`, and `reports/null-progress.json`. The strict golden
comparison was not relaxed. The initial results before these rounds were 363
strict and 408 semantic output matches.

The semantic gate now checks the official executable's pinned checksum and
version, then uses its absolute path for every evaluation. A disappearing oracle
interrupted an intermediate run; the restored 0.33 asset passed its checksum and
the final complete recheck. Missing or incorrect oracle files fail before any
evaluator is run. Invalid intermediate results remain in the reports directory
and are excluded from the scores above.

The executable uses release semantics with a package-only `pklr` optimization
level of 1, debug information disabled, and two build jobs. On this Mac, slow Nix
standard-library path lookups required a byte-identical local sysroot and an APFS
clone of the warm target. All sixty copied library files were SHA-256 verified;
`reports/regex-local-sysroot.json` records their hashes and the wrapper. Validation
used `.target-conformance-fast`. Both experiment targets and that local sysroot
together account for under 6 GiB by `du`, including shared clone blocks counted
twice. Inactive debug incremental generations were reclaimed after validation;
`reports/module-capture-cache-cleanup.json` records the exact path and observed
free-space changes. The preserved standalone evaluators remain usable without
those caches.

The full language rewrite is not complete. Gaps include nullable typed-class expression defaults,
recursive typed Listing defaults, the 0.33 generic-method/reference/`this` type
features, portions of the standard library and renderers, project/packages and
the HTTPS package fixture server, complete evaluator protocol/value identity,
and source-aware diagnostics. Diagnostic value text also does not fully track
which members were already evaluated; `reports/render-unresolved-probes.json`
records a concrete remaining mismatch. Map text also does not retain which nested
object members remain unforced; `reports/map-unresolved-probes.json` records that
case against the preceding Map/module artifact. These are implementation gaps, not features that
can be inferred from two evaluators returning arbitrary errors.

This entire directory is ignored by the parent Nori repository. It contains real
source and test artifacts for continued experimental work, not production
integration or a PR claiming complete conformance.
