# ACP fault catalog

The ways an agent and Nori's harness fall out of step, each as a fault a fake agent can play and a
test that drives it through the real harness ([fake-worlds.md](fake-worlds.md)). A fault is in this
catalog because a real agent committed it. Nothing here runs against a provider.

```sh
cd nori-rs && just faults               # the whole catalog, on the virtual clock
just faults --test claude_faults        # one file
just faults -- a_dying_cli              # one test, by name
```

The tests are `nori-rs/world/tests/*.rs`; the worlds they play are in `nori-rs/fake-agent/worlds/`.
A test passes when Nori does what it must in every fault: no crash, no hang, no tool or turn left open,
a message a person can read (never a provider's JSON body), a session that is still usable, and a
fallback that engages where one is configured.

## Request size

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `oversize-fork` | A child that continues a long history fails with `Invalid 'input[N].content[0].text': string too long` | Nori builds one unbounded text block from the history | Codex | `oversize_context`: `forking_a_long_history_sends_no_block_over_the_provider_limit` |
| `oversize-checkpoint` | The fallback route fails the same way, or `Error running remote compact task: ... string_above_max_length` | Nori hands a whole conversation to a route as one checkpoint block | Codex | `oversize_context`: `a_fallback_checkpoint_sends_no_block_over_the_provider_limit` |
| `refusal-prose` | The person reads `{"type":"error","error":{...}}` | A provider's body shown as it came | Claude, Codex | `oversize_context`: `a_context_refusal_reaches_the_person_as_prose_and_the_session_recovers` |
| `oversize-paste` | A pasted file over the limit fails the turn with raw JSON | The same, from the person's own prompt | Codex | `oversize_context`: `an_oversize_paste_never_shows_the_providers_json` |

## Context and loops

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `context-overflow` | A long session fails with `prompt is too long` in the middle of the work, or the agent forgets the start | Nothing compacted the context before it ran out | any | `context_pressure`: `a_context_over_the_trigger_is_compacted_before_the_next_prompt`, `a_summary_prepared_ahead_of_need_is_the_one_the_swap_uses`, `an_agent_with_its_own_compact_command_is_sent_that_instead_of_a_summary_prompt`, `a_compaction_keeps_the_model_and_the_mode`, `a_failed_automatic_compaction_is_said_once_and_not_tried_again` |
| `doom-loop` | An agent calls the same tool with the same input over and over, turn after turn of spent tokens | A model that does not notice it is stuck | any | `context_pressure`: `an_agent_that_repeats_a_call_is_steered_once_and_then_stopped` |

## Stream and protocol drift

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `unknown-update-kind` | `unknown variant tool_progress`; the turn breaks | A message kind the adapter or client has never seen | Claude | `claude_faults`: `update_kinds_nobody_defined_do_not_break_the_turn` |
| `unknown-content-block` | A chunk with a content block of a new type | The same, one level down | Claude | `claude_faults`: `a_content_block_from_the_future_does_not_break_the_turn` |
| `stale-response`, `dropped-response`, `double-response` | `received response for unknown id`; a turn that never ends, or ends twice | Responses that do not match the requests outstanding | any | `adversarial-faults.pkl` (`drop`, `twice`, `stale`) |
| `late-notification` | `Received event for unknown submission ID`; text, tools or permission requests after `end_turn` or after a cancel | An agent that outlives its turn | Codex, Claude | `carbon-replay.pkl`, `adversarial-faults.pkl` (`detach`) |
| `tool-disorder` | A tool that ends before it starts, starts twice, changes after its end, or never ends | Out-of-order updates | any | `adversarial-faults.pkl` (`disorder`) |

## Turns and children

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `replace-turn` | A prompt sent while a turn runs leaves two turns, or an open tool | The agent's turn policy replaces the running turn and the old one is slow to stop | Claude | `claude_faults`: `steering_into_a_turn_that_is_slow_to_cancel_replaces_it` |
| `permission-vs-cancel` | A permission request that outlives a cancel, or is answered as cancelled when the user did not cancel | A cancel that races a question | Claude, Codex | `claude_faults`: `cancelling_while_a_permission_is_open_ends_the_turn_cleanly` |
| `stream-eof` | `SDK background reader: stream ended unexpectedly`; a turn that never ends, a tool that spins forever | The agent's process dies mid-turn | Claude | `claude_faults`: `a_dying_cli_fails_the_turn_without_hanging_and_without_a_ghost_tool`; the SDK's `cli_exit_tests` play the dying CLI itself |
| `dead-cli-wedge` | After the CLI is killed the next prompt times out, then no request on the connection is answered | The SDK removed the dead query from a `DashMap` while still holding a read guard on it, which blocks the thread forever | Claude | SDK `cli_exit_tests`: `a_cli_that_dies_mid_turn_ends_the_stream_and_the_next_query_resumes_the_conversation` |
| `child-dies` | The parent fails or waits forever for a child that failed | A child session ends in failure | Claude | `claude_faults`: `a_failing_child_does_not_fail_its_parent` |

| `handoff-orphan` | A client that sent a prompt never sees it end when a quota moves the session to another agent; `ask` waits forever | The prompt sent again on the new route had a new request ID | any | `oversize_context`: `a_fallback_checkpoint_sends_no_block_over_the_provider_limit` |
| `close-unsupported` | `session/close` answered "Method not found" (653 times in the logs) | Catalog probes closed their sessions on agents that never offered `session/close` | Codex, OpenCode | `claude_faults`: `an_agent_without_session_close_is_never_sent_one` |

## Errors a person reads

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `provider-prose` | `Your credit balance is too low ...` arrives as `Internal error` | A provider's sentence inside a JSON-RPC internal error | Claude | `claude_faults`: `the_providers_sentence_reaches_the_person`; `nori-acp-kit` `error::tests` |
| `nested-json-error` | The provider's JSON body shown as the message | A JSON body as the `detail` of the error | Claude | `claude_faults`: `a_nested_json_error_body_is_never_shown_raw` |
| `quota-fallback` | A spent quota that does not move the session along | Recovery policy | any | `quota-fallback.pkl` |

## Settings and models

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `model-switch-lies` | `/model` says Opus and the old model answers; `Couldn't select model` | An agent that acknowledges a setting and does something else, or refuses it | Claude, OpenCode | `claude_faults`: `the_model_nori_reports_is_the_model_that_answers` |
| `models-arrive-late` | The model list is empty until the CLI reports it | `session/new` lists nothing and a `config_option_update` fills it in | Claude | `claude_faults`: `models_announced_after_session_new_reach_the_session` |

## Starting and reopening

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `cli-missing` | `Claude Code CLI not found`; `Agent backend unreachable` | The program the agent runs is not installed | Claude | `claude_faults`: `a_missing_cli_is_reported_as_not_installed`; `nori-claude-acp` `types::error::tests` |
| `initialize-never` | `Control request timed out after 60s`; a start that never ends | An agent that stalls before it answers | Claude | `startup_faults`: `an_agent_that_never_answers_initialize_fails_the_start_in_words`; SDK `a_cli_that_exits_before_answering_fails_the_request_at_once_and_says_why` |
| `new-session-never` | `deadline has elapsed` on `session/new` | The same, one request later | Claude, OpenCode | `startup_faults`: `an_agent_that_never_answers_session_new_fails_the_start_in_words` |
| `sessions-locked` | `thread ... already has an active writer`; a resume that goes nowhere | `session/load` and `resume` fail | Codex | `claude_faults`: `a_session_that_cannot_be_reopened_leaves_a_usable_session` |
| `list-unreachable` | `Failed to list ACP sessions (error sending request for url ...)`; Nori does not start | `session/list` fails while a connection is prepared; the catalog is `Unavailable` and a new session starts anyway | OpenCode | `claude_faults`: `an_agent_whose_session_list_is_unreachable_still_starts_a_session` |

## Background jobs

World: `jobs.pkl`, whose jobs run scripted `processes` ([fake-worlds.md](fake-worlds.md#processes)) under the
real registry, wakes and tools. Process time is real time, a few seconds a test.

| Fault | Symptom | Cause class | Agent | Reproduced by |
| --- | --- | --- | --- | --- |
| `job-ready-exit` | A job's ready line and its exit arrive as one wake, or a line is read twice | A job that is ready and then exits non-zero | any | `job_faults`: `a_job_wakes_the_agent_once_when_ready_and_once_when_it_exits` |
| `job-never-ends` | A server nobody wakes the agent for; a stop that wakes it anyway | A job that runs until stopped | any | `job_faults`: `a_job_that_never_finishes_wakes_nobody_and_is_stopped_quietly` |
| `job-flood` | A wake per matching line; a session that stops answering; a read that returns the whole log | A job printing thousands of lines | any | `job_faults`: `a_job_that_floods_output_is_batched_and_the_session_stays_responsive` |
| `job-dies` | `exited 1` with nothing about why | A job that prints on stderr and exits non-zero | any | `job_faults`: `a_job_that_dies_wakes_the_agent_with_its_last_words` |
| `job-unknown-command` | A job that is listed but never ran, or an error without a reason | A command the machine cannot run | any | `job_faults`: `a_command_the_world_runs_nothing_for_fails_to_start` |
| `job-count` | The status line counts a job that ended, or misses one that started | The running-jobs count of `MonitorWire::State` | any | `job_faults`: `the_running_jobs_count_follows_the_jobs` |
| `job-instant-ready` | A job that is ready at once never wakes the agent | A line printed before the watch subscribed | any | `job_faults`: `a_ready_line_printed_at_once_is_not_lost` |

## What is not simulated

Faults inside an adapter that the harness never sees are the adapter's own tests: the SDK's parsing
of the Claude CLI's stream (a new message kind is an `Unrecognized` message, not an error), the
account's `CLAUDE_CONFIG_DIR`, the CLI process the adapter restarts for a new model, and
`connection_cx already set`. The faults above are what each of those looks like from the harness's
side of the ACP wire.

## Adding one

1. Say it in a world (`nori-rs/fake-agent/worlds/`): a scene keyed on a prompt, or a `startup`,
   `limits` or `config_faults` entry. Extend the fake agent only when the world cannot say it.
2. Write the test in `nori-rs/world/tests/`, named for what Nori must do, with the fault id in its
   doc comment.
3. Add a row here.
