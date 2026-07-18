//! Route-menu integration for the engine transaction-proposer autorouter.

use super::EutecticApp;
use crate::chrome::actions::ChromeNotice;
use crate::pick::SemanticId;
use eutectic_core::command::Transaction;
use eutectic_core::id::NetId;
use eutectic_core::route::DesignRules;
use std::collections::BTreeSet;

pub(crate) const AUTOROUTE_NET_KEY: &str = "route:autoroute-net";
pub(crate) const AUTOROUTE_BOARD_KEY: &str = "route:autoroute-board";

impl EutecticApp {
    pub(crate) fn selected_route_nets(&self) -> BTreeSet<NetId> {
        let Ok(doc) = &self.domain.doc else {
            return BTreeSet::new();
        };
        let selection = self.domain.selection.borrow();
        let mut nets = BTreeSet::new();
        for selected in selection.selected() {
            match selected {
                SemanticId::Net(net) | SemanticId::Pour { net, .. } => {
                    nets.insert(net.clone());
                }
                SemanticId::Trace(id) => {
                    if let Some(trace) = doc.traces.get(id) {
                        nets.insert(trace.net.clone());
                    }
                }
                SemanticId::Via(id) => {
                    if let Some(via) = doc.vias.get(id) {
                        nets.insert(via.net.clone());
                    }
                }
                SemanticId::Pin { comp, pin } => {
                    nets.extend(
                        doc.nets
                            .iter()
                            .filter(|(_, facts)| {
                                facts
                                    .members
                                    .iter()
                                    .any(|member| member.comp == *comp && member.pin == *pin)
                            })
                            .map(|(net, _)| net.clone()),
                    );
                }
                SemanticId::Part(_) => {}
            }
        }
        nets
    }

    pub(crate) fn can_autoroute_selection(&self) -> bool {
        !self.selected_route_nets().is_empty()
    }

    pub(crate) fn autoroute_board(&mut self) {
        let result = {
            let Ok(doc) = &self.domain.doc else {
                return;
            };
            eutectic_core::autoroute::autoroute(doc, &self.domain.lib, &DesignRules::default())
        };
        self.commit_autoroute(result);
    }

    pub(crate) fn autoroute_selection(&mut self) {
        let nets = self.selected_route_nets();
        if nets.is_empty() {
            return;
        }
        let result = {
            let Ok(doc) = &self.domain.doc else {
                return;
            };
            eutectic_core::autoroute::autoroute_nets(
                doc,
                &self.domain.lib,
                &DesignRules::default(),
                &nets,
            )
        };
        self.commit_autoroute(result);
    }

    fn commit_autoroute(&mut self, result: eutectic_core::autoroute::AutorouteResult) {
        let routed = result.routed.len();
        let total = routed + result.unrouted.len();
        if !result.commands.is_empty()
            && let Err(error) = self.commit_edit(Transaction(result.commands), "autoroute")
        {
            self.domain.edit.error = Some(error.clone());
            *self.chrome_notice.borrow_mut() =
                Some(ChromeNotice::error(format!("autoroute failed: {error}")));
            return;
        }
        *self.chrome_notice.borrow_mut() = Some(ChromeNotice::success(format!(
            "autoroute: {routed}/{total} nets routed"
        )));
    }
}
