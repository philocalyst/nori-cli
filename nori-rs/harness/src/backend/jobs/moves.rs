//! Jobs of a session that moved here from another machine (`portable::import` journals each
//! arrival as `moved-in`). A job record says `local` for the computer that wrote it, so a local job
//! written before a move ran on the machine the session left: the `from` of the first arrival after
//! it. Records after the latest arrival are this computer's own. A chain of moves (A to B to C)
//! sorts each record to the machine it was written on.

use std::collections::HashMap;

use nori_machines::Roster;
use nori_machines::Target;
use nori_machines::jobs::JobId;
use nori_machines::jobs::Place;
use serde::Deserialize;

use super::journal;
use super::journal::Record;
use crate::portable::MOVED_IN;

/// Where the session was before one move.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct MovedIn {
    /// The origin's hostname.
    from: String,
    /// The origin's name in the roster it was moved with, if it had one.
    #[serde(default)]
    from_machine: Option<String>,
}

/// A local job the journal wrote on another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Elsewhere {
    from: String,
    from_machine: Option<String>,
}

impl Elsewhere {
    /// Where the job runs as seen from here, or why it cannot be reached from here.
    pub(super) fn place(&self, roster: &Roster) -> Result<Place, String> {
        let unreachable = || format!("left running on {}; not reachable from here", self.from);
        let name = self.from_machine.as_deref().ok_or_else(unreachable)?;
        match roster.resolve(Some(name)) {
            Ok(Target::Remote(machine)) => Ok(Place::Machine(machine.name.clone())),
            // The session came back to the computer it left.
            Ok(Target::Local) => Ok(Place::Local),
            Err(_) => Err(unreachable()),
        }
    }
}

/// The job records of `commands` (the journal's `job` and `moved-in` commands, oldest first), and
/// the local jobs among them that were written on another machine.
pub(super) fn sort_out(
    commands: Vec<(String, String)>,
) -> (Vec<Record>, HashMap<JobId, Elsewhere>) {
    let mut records = Vec::new();
    let mut elsewhere = HashMap::new();
    // Local jobs written since the last arrival: where they ran is the next arrival's `from`.
    let mut unplaced: Vec<JobId> = Vec::new();
    for (name, payload) in commands {
        if name == MOVED_IN {
            let Ok(moved) = serde_json::from_str::<MovedIn>(&payload) else {
                continue;
            };
            for id in unplaced.drain(..) {
                elsewhere.insert(
                    id,
                    Elsewhere {
                        from: moved.from.clone(),
                        from_machine: moved.from_machine.clone(),
                    },
                );
            }
        } else if name == journal::COMMAND
            && let Ok(record) = serde_json::from_str::<Record>(&payload)
        {
            if let Record::Started { record: job, .. } = &record
                && job.place == Place::Local
            {
                unplaced.push(job.id.clone());
            }
            records.push(record);
        }
    }
    (records, elsewhere)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::backend::jobs::JobOrigin;
    use crate::backend::jobs::JobWake;

    fn started(id: &str, place: Place) -> (String, String) {
        let record = Record::Started {
            record: nori_machines::jobs::JobRecord {
                id: serde_json::from_value(serde_json::json!(id)).expect("id"),
                place,
                label: id.to_owned(),
                command: "true".to_owned(),
                pid: 1,
                boot: "b".to_owned(),
                started_at: 0,
            },
            origin: JobOrigin::Agent,
            wake: JobWake::default(),
        };
        (
            journal::COMMAND.to_owned(),
            serde_json::to_string(&record).expect("record"),
        )
    }

    fn moved(from: &str, machine: Option<&str>) -> (String, String) {
        (
            MOVED_IN.to_owned(),
            serde_json::json!({"from": from, "from_machine": machine, "cwd": "/w", "at": "now"})
                .to_string(),
        )
    }

    #[test]
    fn each_local_job_belongs_to_the_machine_it_was_written_on() {
        let remote: Place =
            serde_json::from_value(serde_json::json!({"kind": "machine", "name": "h1"}))
                .expect("place");
        let (records, elsewhere) = sort_out(vec![
            started("a", Place::Local),
            started("r", remote),
            moved("alpha.local", Some("alpha")),
            started("b", Place::Local),
            moved("beta.local", None),
            started("c", Place::Local),
        ]);
        assert_eq!(records.len(), 4);
        let mut placed: Vec<(String, Elsewhere)> = elsewhere
            .into_iter()
            .map(|(job, elsewhere)| (job.to_string(), elsewhere))
            .collect();
        placed.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            placed,
            vec![
                (
                    "a".to_owned(),
                    Elsewhere {
                        from: "alpha.local".to_owned(),
                        from_machine: Some("alpha".to_owned()),
                    }
                ),
                (
                    "b".to_owned(),
                    Elsewhere {
                        from: "beta.local".to_owned(),
                        from_machine: None,
                    }
                ),
            ]
        );
    }

    #[test]
    fn an_origin_this_roster_does_not_know_is_not_reachable() {
        let elsewhere = Elsewhere {
            from: "beta.local".to_owned(),
            from_machine: None,
        };
        assert_eq!(
            elsewhere.place(&Roster::empty()),
            Err("left running on beta.local; not reachable from here".to_owned())
        );
    }
}
