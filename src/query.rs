//! A minimal demand-driven, memoized query engine for the derived tier (tier 3).
//!
//! This is a hand-rolled, deliberately small version of the Salsa/rust-analyzer
//! idea, built so the mechanics are explicit:
//!
//!   - Queries are memoized; each memo records the *dependencies* it read.
//!   - Each memo carries `verified_at` (last revision we confirmed it current)
//!     and `changed_at` (last revision its *value* actually changed).
//!   - Re-evaluating first tries to *validate* without recomputing: if every
//!     dependency is unchanged, just restamp `verified_at`.
//!   - When a query is recomputed, the new value is compared to the old. If equal,
//!     `changed_at` is left alone — this is **early cutoff**: a recompute whose
//!     result didn't change does not invalidate the queries that depend on it.
//!
//! Dependencies here are recorded explicitly by each query body rather than via
//! automatic read-interception; production would auto-track. The invalidation
//! algorithm is the real thing.
//!
//! Only tier-3 (pure, deterministic, cheap-to-compare) lives here. Tier-2 solver
//! state is deliberately NOT a query — see docs/architecture.md.

use crate::doc::{Doc, InputId, PinRef};
use crate::id::NetId;
use crate::part::{PartLib, PinRole};
use crate::route::{check_drc, DesignRules, Violation};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// Resolved netlist: each net's pins paired with their electrical roles.
    Netlist,
    /// Electrical-rules check, computed *from* the resolved netlist.
    Erc,
    /// Design-rules check over the routed copper: clearance, min width, ratsnest.
    Drc,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryValue {
    Netlist(BTreeMap<NetId, Vec<(PinRef, PinRole)>>),
    Erc(Vec<String>),
    Drc(Vec<Violation>),
}

impl QueryValue {
    pub fn as_erc(&self) -> &[String] {
        match self {
            QueryValue::Erc(v) => v,
            _ => panic!("not an Erc value"),
        }
    }
    pub fn as_netlist(&self) -> &BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
        match self {
            QueryValue::Netlist(m) => m,
            _ => panic!("not a Netlist value"),
        }
    }
    pub fn as_drc(&self) -> &[Violation] {
        match self {
            QueryValue::Drc(v) => v,
            _ => panic!("not a Drc value"),
        }
    }
}

#[derive(Clone, Debug)]
enum Dep {
    Input { which: InputId, rev: u64 },
    Query { key: Key, changed_at: u64 },
}

struct Memo {
    value: QueryValue,
    deps: Vec<Dep>,
    verified_at: u64,
    changed_at: u64,
}

#[derive(Default)]
pub struct Engine {
    memos: BTreeMap<Key, Memo>,
    current_rev: u64,
    /// How many times each query body actually executed — for the demo/tests to
    /// prove that unaffected queries are skipped.
    pub recompute_counts: BTreeMap<Key, u32>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self, key: Key) -> u32 {
        self.recompute_counts.get(&key).copied().unwrap_or(0)
    }

    /// Query a value, bringing the engine's notion of "now" up to the document's
    /// latest input revision first.
    pub fn query(&mut self, doc: &Doc, lib: &PartLib, key: Key) -> QueryValue {
        self.current_rev = doc.conn_rev.max(doc.geom_rev).max(doc.route_rev);
        self.eval(doc, lib, key);
        self.memos[&key].value.clone()
    }

    /// Ensure `key`'s memo is verified at the current revision.
    fn eval(&mut self, doc: &Doc, lib: &PartLib, key: Key) {
        if let Some(m) = self.memos.get(&key)
            && m.verified_at == self.current_rev
        {
            return; // already known current this revision
        }
        if self.try_validate(doc, lib, key) {
            return; // deps unchanged -> reused without recompute
        }
        self.recompute(doc, lib, key);
    }

    /// Attempt to mark `key` current by checking its recorded dependencies.
    /// Returns true if validated (no recompute needed).
    fn try_validate(&mut self, doc: &Doc, lib: &PartLib, key: Key) -> bool {
        let deps = match self.memos.get(&key) {
            Some(m) => m.deps.clone(),
            None => return false,
        };
        for dep in &deps {
            match dep {
                Dep::Input { which, rev } => {
                    if doc.input_rev(*which) != *rev {
                        return false;
                    }
                }
                Dep::Query { key: sub, changed_at } => {
                    self.eval(doc, lib, *sub);
                    if self.memos[sub].changed_at > *changed_at {
                        return false; // a dependency's value moved
                    }
                }
            }
        }
        // Everything we read is unchanged: restamp without recomputing.
        if let Some(m) = self.memos.get_mut(&key) {
            m.verified_at = self.current_rev;
        }
        true
    }

    fn recompute(&mut self, doc: &Doc, lib: &PartLib, key: Key) {
        *self.recompute_counts.entry(key).or_insert(0) += 1;
        let mut deps = Vec::new();
        let value = self.compute(doc, lib, key, &mut deps);
        // Early cutoff: if the recomputed value equals the prior one, keep the old
        // `changed_at` so dependents stay valid.
        let changed_at = match self.memos.get(&key) {
            Some(m) if m.value == value => m.changed_at,
            _ => self.current_rev,
        };
        self.memos.insert(
            key,
            Memo { value, deps, verified_at: self.current_rev, changed_at },
        );
    }

    /// The query bodies. Each records what it reads into `deps`.
    fn compute(&mut self, doc: &Doc, lib: &PartLib, key: Key, deps: &mut Vec<Dep>) -> QueryValue {
        match key {
            Key::Netlist => {
                // Reads connectivity only — geometry edits never reach here.
                deps.push(Dep::Input {
                    which: InputId::Connectivity,
                    rev: doc.input_rev(InputId::Connectivity),
                });
                let mut out: BTreeMap<NetId, Vec<(PinRef, PinRole)>> = BTreeMap::new();
                for (nid, net) in &doc.nets {
                    let mut pins = Vec::new();
                    for pr in &net.members {
                        let role = doc
                            .components
                            .get(&pr.comp)
                            .and_then(|c| lib.get(&c.part))
                            .and_then(|pd| pd.pin_role(&pr.pin))
                            .unwrap_or(PinRole::Passive);
                        pins.push((pr.clone(), role));
                    }
                    out.insert(nid.clone(), pins);
                }
                QueryValue::Netlist(out)
            }
            Key::Erc => {
                // Depends on the *resolved netlist*, not on raw inputs. This is the
                // firewall: when Netlist recomputes but its value is unchanged,
                // this query is skipped.
                self.eval(doc, lib, Key::Netlist);
                deps.push(Dep::Query {
                    key: Key::Netlist,
                    changed_at: self.memos[&Key::Netlist].changed_at,
                });
                let nl = self.memos[&Key::Netlist].value.as_netlist().clone();
                let mut errs = Vec::new();
                for (nid, pins) in &nl {
                    let drivers = pins.iter().filter(|(_, r)| r.is_driver()).count();
                    if drivers >= 2 {
                        errs.push(format!("net `{nid}`: {drivers} drivers contend"));
                    }
                }
                QueryValue::Erc(errs)
            }
            Key::Drc => {
                // DRC reads three things, recorded as dependencies:
                //   - the routed copper (Routing input) — what is being checked;
                //   - placement geometry (Geometry input) — pads move with parts,
                //     so clearance/ratsnest incidence depends on positions;
                //   - the resolved netlist (Netlist query) — fixes which pins each
                //     net must join for the ratsnest, and groups copper by net.
                // Recording Netlist as a *query* dependency (not raw Connectivity)
                // means a connectivity edit whose resolved netlist is unchanged is
                // firewalled by early cutoff — DRC is not recomputed.
                deps.push(Dep::Input {
                    which: InputId::Routing,
                    rev: doc.input_rev(InputId::Routing),
                });
                deps.push(Dep::Input {
                    which: InputId::Geometry,
                    rev: doc.input_rev(InputId::Geometry),
                });
                self.eval(doc, lib, Key::Netlist);
                deps.push(Dep::Query {
                    key: Key::Netlist,
                    changed_at: self.memos[&Key::Netlist].changed_at,
                });
                let nl = self.memos[&Key::Netlist].value.as_netlist().clone();
                let rules = DesignRules::default();
                QueryValue::Drc(check_drc(doc, lib, &nl, &rules))
            }
        }
    }
}
