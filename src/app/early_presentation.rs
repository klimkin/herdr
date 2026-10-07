//! Demand-driven extra presentation, with no token or expiry wake source.

use std::time::{Duration, Instant};

use crate::latency_experiments::PresentationPolicy;

const INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OriginLease {
    pub(crate) client_id: u64,
    pub(crate) projection_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalPlacement {
    pub(crate) workspace_id: String,
    pub(crate) tab_root: crate::layout::PaneId,
    pub(crate) pane_id: crate::layout::PaneId,
    pub(crate) public_pane_id: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Work {
    Action {
        workspace_id: String,
        label: String,
        request_id: String,
    },
    Terminal {
        placement: TerminalPlacement,
        terminal_id: crate::terminal::TerminalId,
        runtime_instance: u64,
        baseline_revision: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Opportunity {
    pub(crate) id: u64,
    pub(crate) accepted_at: Instant,
    pub(crate) expires_at: Instant,
    pub(crate) origin: Option<OriginLease>,
    pub(crate) work: Work,
    attempted: bool,
    #[cfg(feature = "latency-prof")]
    diagnostic_times: (u64, u64),
}

impl Opportunity {
    fn target_key(&self) -> &str {
        match &self.work {
            Work::Action { workspace_id, .. } => workspace_id,
            Work::Terminal { terminal_id, .. } => terminal_id.as_str(),
        }
    }
}

pub(crate) struct EarlyPresentation {
    policy: PresentationPolicy,
    opportunities: Vec<Opportunity>,
    next_id: u64,
    last_admission: Option<Instant>,
    pub(crate) origin: Option<OriginLease>,
}

impl EarlyPresentation {
    pub(crate) fn new(policy: PresentationPolicy) -> Self {
        Self {
            policy,
            opportunities: Vec::new(),
            next_id: 1,
            last_admission: None,
            origin: None,
        }
    }

    pub(crate) fn actions_enabled(&self) -> bool {
        self.policy == PresentationPolicy::ActionFull
    }

    pub(crate) fn accepted_action(
        &mut self,
        workspace_id: String,
        label: String,
        request_id: String,
        now: Instant,
    ) {
        if !self.actions_enabled() {
            return;
        }
        self.expire(now);
        if let Some(existing) = self.opportunities.iter_mut().find(|opportunity| {
            matches!(&opportunity.work, Work::Action { workspace_id: old, .. } if old == &workspace_id)
        }) {
            crate::latency_prof::record("opportunity.superseded", existing.id, crate::latency_prof::bytes_id(request_id.as_bytes()));
            existing.work = Work::Action { workspace_id, label, request_id };
            existing.origin = self.origin;
            record_origin(existing.id, existing.origin);
            #[cfg(feature = "latency-prof")]
            record_opportunity_times(existing.id, existing.diagnostic_times);
            // The first accepted grant owns expiry/priority. A coalesced action
            // cannot restore a spent admission or extend its lifetime.
            return;
        }
        let Some(next_id) = self.next_id.checked_add(1) else {
            tracing::warn!("early presentation opportunity identity exhausted");
            return;
        };
        let id = self.next_id;
        self.next_id = next_id;
        crate::latency_prof::record_at(
            "opportunity.action_granted",
            id,
            crate::latency_prof::bytes_id(label.as_bytes()),
            crate::latency_prof::bytes_id(request_id.as_bytes()),
            crate::latency_prof::now(),
        );
        crate::latency_prof::record(
            "opportunity.action_target",
            id,
            crate::latency_prof::bytes_id(workspace_id.as_bytes()),
        );
        record_origin(id, self.origin);
        #[cfg(feature = "latency-prof")]
        let diagnostic_times = diagnostic_opportunity_times(now);
        #[cfg(feature = "latency-prof")]
        record_opportunity_times(id, diagnostic_times);
        self.opportunities.push(Opportunity {
            id,
            accepted_at: now,
            expires_at: now + INTERVAL,
            origin: self.origin,
            work: Work::Action {
                workspace_id,
                label,
                request_id,
            },
            attempted: false,
            #[cfg(feature = "latency-prof")]
            diagnostic_times,
        });
    }

    pub(crate) fn oldest_ready(
        &mut self,
        now: Instant,
        mut ready: impl FnMut(&Opportunity) -> bool,
    ) -> Option<Opportunity> {
        self.expire(now);
        self.opportunities
            .iter()
            .filter(|o| !o.attempted && ready(o))
            .min_by(|left, right| {
                left.accepted_at
                    .cmp(&right.accepted_at)
                    .then_with(|| left.target_key().cmp(right.target_key()))
            })
            .cloned()
    }

    pub(crate) fn admit(&mut self, id: u64, now: Instant, ordinary_due: bool) -> bool {
        if ordinary_due {
            return false;
        }
        if self
            .last_admission
            .is_some_and(|last| now.saturating_duration_since(last) < INTERVAL)
        {
            crate::latency_prof::record("opportunity.token_denied", id, 0);
            return false;
        }
        let Some(opportunity) = self
            .opportunities
            .iter_mut()
            .find(|o| o.id == id && !o.attempted && now < o.expires_at)
        else {
            return false;
        };
        opportunity.attempted = true;
        self.last_admission = Some(now);
        #[cfg(feature = "latency-prof")]
        if crate::latency_prof::active() {
            let anchor = Instant::now();
            let anchor_ns = crate::latency_prof::now();
            let charged_ns = if now >= anchor {
                anchor_ns.saturating_add(now.duration_since(anchor).as_nanos() as u64)
            } else {
                anchor_ns.saturating_sub(anchor.duration_since(now).as_nanos() as u64)
            };
            crate::latency_prof::record_at("opportunity.charged_at", id, charged_ns, 0, anchor_ns);
        }
        true
    }

    pub(crate) fn acknowledge_action(
        &mut self,
        snapshot: &crate::protocol::ClientShellSnapshot,
        surface_projection_revision: u64,
        client_id: u64,
        context: crate::latency_prof::TraceContext,
    ) -> bool {
        if !self.actions_enabled() || snapshot.revision != surface_projection_revision {
            return false;
        }
        self.expire(Instant::now());
        let mut satisfied = false;
        self.opportunities.retain(|opportunity| {
            let Work::Action {
                workspace_id,
                label,
                request_id,
            } = &opportunity.work
            else {
                return true;
            };
            if !snapshot.workspaces.iter().any(|workspace| {
                &workspace.workspace_id == workspace_id && &workspace.label == label
            }) {
                return true;
            }
            satisfied = true;
            crate::latency_prof::record(
                "opportunity.action_satisfied_request",
                opportunity.id,
                crate::latency_prof::bytes_id(request_id.as_bytes()),
            );
            #[cfg(feature = "latency-prof")]
            crate::latency_prof::record_context_at(
                "opportunity.action_enqueued",
                opportunity.id,
                snapshot.revision,
                client_id,
                crate::latency_prof::now(),
                context,
            );
            #[cfg(not(feature = "latency-prof"))]
            let _ = (client_id, context);
            false
        });
        satisfied
    }

    pub(crate) fn terminal_enabled(&self) -> bool {
        matches!(
            self.policy,
            PresentationPolicy::Target | PresentationPolicy::TargetAll
        )
    }

    pub(crate) fn pending(&self) -> bool {
        !self.opportunities.is_empty()
    }
    pub(crate) fn contains(&self, id: u64) -> bool {
        self.opportunities
            .iter()
            .any(|opportunity| opportunity.id == id)
    }

    pub(crate) fn tracks_terminal(&self, terminal_id: &crate::terminal::TerminalId) -> bool {
        self.opportunities.iter().any(|opportunity|matches!(&opportunity.work,Work::Terminal{terminal_id:target,..} if target==terminal_id))
    }

    pub(crate) fn accepted_terminal(
        &mut self,
        terminal_id: crate::terminal::TerminalId,
        placement: TerminalPlacement,
        runtime_instance: u64,
        baseline_revision: u64,
        origin: Option<OriginLease>,
        now: Instant,
    ) -> Option<Opportunity> {
        if !self.terminal_enabled() || runtime_instance == 0 || !baseline_revision.is_multiple_of(2)
        {
            return None;
        }
        self.expire(now);
        if let Some(existing) = self.opportunities.iter().find(|opportunity| {
            matches!(&opportunity.work, Work::Terminal { terminal_id: old, runtime_instance: old_instance, .. }
                if old == &terminal_id && *old_instance == runtime_instance)
        }) {
            crate::latency_prof::record("opportunity.coalesced", existing.id, baseline_revision);
            return Some(existing.clone());
        }
        // A replacement runtime cannot share a prior counter lifetime.
        self.opportunities.retain(|opportunity| {
            !matches!(&opportunity.work,
            Work::Terminal { terminal_id: old, .. } if old == &terminal_id)
        });
        let next_id = self.next_id.checked_add(1)?;
        let id = self.next_id;
        self.next_id = next_id;
        crate::latency_prof::record_runtime_at(
            "opportunity.terminal_granted",
            id,
            baseline_revision,
            crate::latency_prof::bytes_id(terminal_id.as_str().as_bytes()),
            crate::latency_prof::now(),
            runtime_instance,
        );
        crate::latency_prof::record_runtime_at(
            "opportunity.target_pane",
            id,
            crate::latency_prof::bytes_id(placement.public_pane_id.as_bytes()),
            0,
            crate::latency_prof::now(),
            runtime_instance,
        );
        #[cfg(feature = "latency-prof")]
        let diagnostic_times = diagnostic_opportunity_times(now);
        #[cfg(feature = "latency-prof")]
        record_opportunity_times(id, diagnostic_times);
        let opportunity = Opportunity {
            id,
            accepted_at: now,
            expires_at: now + INTERVAL,
            origin,
            work: Work::Terminal {
                placement,
                terminal_id,
                runtime_instance,
                baseline_revision,
            },
            attempted: false,
            #[cfg(feature = "latency-prof")]
            diagnostic_times,
        };
        self.opportunities.push(opportunity.clone());
        Some(opportunity)
    }

    pub(crate) fn acknowledge_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        runtime_instance: u64,
        revision: u64,
        client_id: u64,
        context: crate::latency_prof::TraceContext,
    ) -> bool {
        if !self.terminal_enabled()
            || runtime_instance == 0
            || revision == 0
            || !revision.is_multiple_of(2)
        {
            return false;
        }
        self.expire(Instant::now());
        let mut satisfied = false;
        self.opportunities.retain(|opportunity| {
            let Work::Terminal {
                terminal_id: target,
                runtime_instance: instance,
                baseline_revision,
                ..
            } = &opportunity.work
            else {
                return true;
            };
            if target != terminal_id
                || *instance != runtime_instance
                || revision <= *baseline_revision
            {
                return true;
            }
            satisfied = true;
            #[cfg(feature = "latency-prof")]
            crate::latency_prof::record_context_at(
                "opportunity.terminal_enqueued",
                opportunity.id,
                revision,
                client_id,
                crate::latency_prof::now(),
                crate::latency_prof::TraceContext {
                    runtime_instance,
                    ..context
                },
            );
            #[cfg(not(feature = "latency-prof"))]
            let _ = (client_id, context);
            false
        });
        satisfied
    }

    pub(crate) fn prune_terminals(&mut self, now: Instant, mut valid: impl FnMut(&Work) -> bool) {
        self.expire(now);
        self.opportunities.retain(|opportunity| {
            !matches!(opportunity.work, Work::Terminal { .. }) || valid(&opportunity.work)
        });
    }

    pub(crate) fn cancel_terminal(&mut self, terminal_id: &crate::terminal::TerminalId) {
        self.opportunities.retain(|opportunity| !matches!(&opportunity.work,Work::Terminal{terminal_id:target,..} if target==terminal_id));
    }

    pub(crate) fn cancel_client(&mut self, client_id: u64) {
        self.opportunities.retain(|opportunity| {
            if opportunity
                .origin
                .is_none_or(|origin| origin.client_id != client_id)
            {
                return true;
            }
            crate::latency_prof::record("opportunity.revoked", opportunity.id, client_id);
            false
        });
    }

    fn expire(&mut self, now: Instant) {
        self.opportunities.retain(|opportunity| {
            if now < opportunity.expires_at {
                return true;
            }
            crate::latency_prof::record("opportunity.expired", opportunity.id, 0);
            false
        });
    }
}

impl super::App {
    /// Captures a fresh P decision while preserving B's existing time sample.
    pub(crate) fn early_action_ready(
        &mut self,
        now: Instant,
        mut origin_valid: impl FnMut(OriginLease) -> bool,
    ) -> Option<Opportunity> {
        let state = &self.state;
        self.early_presentation.oldest_ready(now, |opportunity| {
            if opportunity
                .origin
                .is_some_and(|origin| !origin_valid(origin))
            {
                return false;
            }

            let Work::Action {
                workspace_id,
                label,
                ..
            } = &opportunity.work
            else {
                return false;
            };
            state.workspaces.iter().any(|workspace| {
                &workspace.id == workspace_id && workspace.custom_name.as_ref() == Some(label)
            })
        })
    }
}

fn record_origin(id: u64, origin: Option<OriginLease>) {
    crate::latency_prof::record_at(
        "opportunity.origin",
        id,
        origin.map_or(0, |origin| origin.client_id),
        origin.map_or(0, |origin| origin.projection_revision),
        crate::latency_prof::now(),
    );
}

#[cfg(feature = "latency-prof")]
fn diagnostic_opportunity_times(accepted: Instant) -> (u64, u64) {
    if !crate::latency_prof::active() {
        return (0, 0);
    }
    let anchor = Instant::now();
    let anchor_ns = crate::latency_prof::now();
    let accepted_ns = if accepted >= anchor {
        anchor_ns.saturating_add(accepted.duration_since(anchor).as_nanos() as u64)
    } else {
        anchor_ns.saturating_sub(anchor.duration_since(accepted).as_nanos() as u64)
    };
    (
        accepted_ns,
        accepted_ns.saturating_add(INTERVAL.as_nanos() as u64),
    )
}

#[cfg(feature = "latency-prof")]
fn record_opportunity_times(id: u64, times: (u64, u64)) {
    if times.0 == 0 {
        return;
    }
    let ns = crate::latency_prof::now();
    crate::latency_prof::record_at("opportunity.accepted_at", id, times.0, 0, ns);
    crate::latency_prof::record_at("opportunity.expires_at", id, times.1, 0, ns);
}
