# Resilience

What Nori does when the world misbehaves: the disk fills, the network goes away for hours, a
provider's usage limit runs out, a machine or Nori itself dies. The rule everywhere: the session
keeps going, nothing is lost or corrupted, and the person is told once, in one line, with what
happens next.

## Disk full

- **Journal.** A conversation's journal record that cannot be written is held in memory (up to
  256 MiB per session) and written, in order, before anything newer once writing works. A write
  cut short leaves an interrupted record that readers skip; the journal is re-read before the next
  write instead of refusing all later ones.
- **Transcript view.** The JSONL transcript is a view of the journal: a line that cannot be
  written is cut off and skipped, never glued to the next one.
- **Before it fills.** The TUI looks at the free space in Nori's home at start and every five
  minutes; under 1 GiB it says once that the disk is nearly full.
- **Telling.** `nori_harness::storage_health` is one process-wide state (`Ok`, `Low` with the
  free bytes, or `Failing` with what, why and whether the disk is full). The TUI says once "The disk is full: this conversation
  is not being saved right now… Nothing stops", and once "Saving again: nothing was lost".

## Long disconnects

- A network failure is retried by the recovery policy, then waited out: Nori keeps trying, at most
  five minutes apart, until the network is back or the person presses Esc. The retry row shows
  "attempt N" with no limit (`RetryScheduled.max_attempts` 0).
- Commands and jobs on other machines survive a dropped connection: they run detached there, and
  their followers reconnect with backoff (`unreachable` meanwhile).

## Usage limits

When a provider's 5-hour or weekly limit is spent (`QuotaExhausted`), the recovery policy moves the
work to another route if one can take it. When none can and the provider said when the limit
resets, the turn waits for the reset instead of failing: one line says "Usage limit reached on
ROUTE; this turn continues when it resets, at Thu 15:00 (Esc stops waiting)", the retry row counts
down, and at the reset (plus 30 s) the same request goes again on the same route, which is no
longer excluded. A limit with no known reset fails as before. The wait lives in the running
Nori: after a restart, the resumed conversation offers the unanswered prompt back.

## Crashes

- **Nori.** A conversation's journal is complete up to the last record written; an unfinished turn
  is replayed with its open tool calls closed as failed, and an unanswered prompt is offered back.
- **The agent.** When the agent dies under a running session (its process exits, its
  connection breaks), Nori says so in one line, restarts it and continues the conversation the
  way a resume does (the agent's own session when it still has it, else a new one handed the
  conversation). Three deaths in ten minutes stop the restarts; `/resume` continues later.
- **A machine.** A job whose machine restarted reads as `lost` (its boot changed); one whose
  machine is down reads as `unreachable` and is adopted again when it answers.
