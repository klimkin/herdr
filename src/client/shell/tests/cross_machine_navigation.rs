//! Reproducer: relative workspace navigation loses key presses while focus moves between
//! machines.
//!
//! The harness drives the real client-shell keybinding path, the runtime dispatch and
//! activation functions, and two fake endpoint servers. Server events are delivered one at a
//! time with the same routing the client event loop uses, so a test can press a key at an exact
//! point of the cross-machine surface handoff.

use super::*;
use crate::api::schema::{Method, Request, ResponseResult, SuccessResponse};
use crate::client::endpoint::{
    accepts_endpoint_message, ClientEndpointId, ClientEndpointStatus, EndpointNegotiation,
    EndpointRegistry, EndpointTransport, PendingEndpointActivation, ProfileId, SavedSshEndpoint,
    SurfaceActivationProgress,
};
use crate::client::endpoint_commands::EndpointCommands;
use crate::client::shell_runtime::{
    begin_endpoint_activation, complete_endpoint_activation, dispatch_client_shell_actions,
    finish_client_shell_input, install_client_shell_snapshot, rollback_endpoint_activation,
};
use crate::client::{ClientLoopEvent, ClientState};
use crate::protocol::{ClientMessage, ClientSurfaceSize, ServerMessage};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type SentMessages = Arc<Mutex<Vec<ClientMessage>>>;

#[derive(Clone)]
struct RecordingTransport(SentMessages);

impl EndpointTransport for RecordingTransport {
    fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
        self.0.lock().unwrap().push(message.clone());
        Ok(())
    }
}

fn negotiation() -> EndpointNegotiation {
    EndpointNegotiation::new(
        vec!["client_shell.surface.set".into()],
        vec![
            crate::protocol::endpoint::SURFACE_INTEREST_CAPABILITY.into(),
            crate::protocol::endpoint::PRESENTATION_EFFECTS_FENCE_CAPABILITY.into(),
        ],
    )
}

fn remote_profile() -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    }
}

enum ServerEvent {
    Response {
        request_id: String,
        result: Box<ResponseResult>,
    },
    Snapshot(Box<ClientShellSnapshot>),
    Surface(Box<PaneSurfaceFrame>),
    EffectsReady(String),
}

/// The protocol-visible behavior of one endpoint server: request responses, snapshots, pane
/// surfaces, and the presentation-effects fence.
struct FakeServer {
    endpoint_id: ClientEndpointId,
    generation: u64,
    boot_id: String,
    workspaces: Vec<String>,
    focused: String,
    revision: u64,
    surface_active: bool,
    geometry: ClientSurfaceSize,
    inbox: SentMessages,
    read: usize,
    outbox: VecDeque<ServerEvent>,
}

impl FakeServer {
    fn new(
        endpoint_id: ClientEndpointId,
        generation: u64,
        boot_id: &str,
        workspaces: &[&str],
        focused: &str,
        surface_active: bool,
        geometry: ClientSurfaceSize,
    ) -> Self {
        Self {
            endpoint_id,
            generation,
            boot_id: boot_id.into(),
            workspaces: workspaces.iter().map(|id| (*id).to_owned()).collect(),
            focused: focused.into(),
            revision: 1,
            surface_active,
            geometry,
            inbox: Arc::new(Mutex::new(Vec::new())),
            read: 0,
            outbox: VecDeque::new(),
        }
    }

    fn snapshot(&self) -> ClientShellSnapshot {
        let template = snapshot();
        let mut projected = template.clone();
        projected.boot_id = self.boot_id.clone();
        projected.revision = self.revision;
        projected.focused_workspace_id = Some(self.focused.clone());
        projected.focused_tab_id = Some(format!("{}:tab", self.focused));
        projected.focused_pane_id = Some(format!("{}:pane", self.focused));
        projected.workspaces = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace_id)| {
                let mut workspace = template.workspaces[0].clone();
                workspace.workspace_id = workspace_id.clone();
                workspace.active_tab_id = format!("{workspace_id}:tab");
                workspace.number = index + 1;
                workspace.label = workspace_id.clone();
                workspace.focused = *workspace_id == self.focused;
                workspace
            })
            .collect();
        projected.tabs = self
            .workspaces
            .iter()
            .map(|workspace_id| {
                let mut tab = template.tabs[0].clone();
                tab.tab_id = format!("{workspace_id}:tab");
                tab.workspace_id = workspace_id.clone();
                tab.focused = *workspace_id == self.focused;
                tab
            })
            .collect();
        projected.panes = self
            .workspaces
            .iter()
            .map(|workspace_id| {
                let mut pane = template.panes[0].clone();
                pane.pane_id = format!("{workspace_id}:pane");
                pane.workspace_id = workspace_id.clone();
                pane.tab_id = format!("{workspace_id}:tab");
                pane.focused = *workspace_id == self.focused;
                pane
            })
            .collect();
        projected
    }

    fn surface(&self) -> PaneSurfaceFrame {
        let area = Rect::new(0, 0, self.geometry.cols, self.geometry.rows);
        let rect = SurfaceRect {
            x: 0,
            y: 0,
            width: self.geometry.cols,
            height: self.geometry.rows,
        };
        PaneSurfaceFrame {
            boot_id: self.boot_id.clone(),
            projection_revision: self.revision,
            surface_revision: self.revision,
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&Buffer::empty(area), None, &[]),
            panes: vec![PaneSurfacePane {
                pane_id: format!("{}:pane", self.focused),
                content_revision: self.revision,
                rect,
                inner_rect: rect,
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: Vec::new(),
            popup: None,
            graphics: crate::protocol::SurfaceGraphicsScene::default(),
        }
    }

    fn publish(&mut self) {
        self.outbox
            .push_back(ServerEvent::Snapshot(Box::new(self.snapshot())));
        if self.surface_active {
            self.outbox
                .push_back(ServerEvent::Surface(Box::new(self.surface())));
        }
    }

    /// Consume every client message written since the last call and queue the server's replies.
    fn pump(&mut self) {
        let messages = self.inbox.lock().unwrap()[self.read..].to_vec();
        self.read += messages.len();
        for message in messages {
            match message {
                ClientMessage::ClientShellResize { surface_size, .. } => {
                    self.geometry = surface_size;
                }
                ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                    assert_eq!(boot_id, self.boot_id, "request addressed to another boot");
                    let request: Request = serde_json::from_str(&request).unwrap();
                    match request.method {
                        Method::ClientShellSurfaceSet(params) => {
                            self.surface_active = params.active;
                            self.revision += 1;
                            self.outbox.push_back(ServerEvent::Response {
                                request_id: request.id,
                                result: Box::new(ResponseResult::ClientShellSurfaceSet {
                                    active: params.active,
                                    projection_revision: self.revision,
                                }),
                            });
                            if params.active {
                                self.publish();
                            }
                        }
                        Method::WorkspaceFocus(target) => {
                            assert!(self.workspaces.contains(&target.workspace_id));
                            self.focused = target.workspace_id.clone();
                            self.revision += 1;
                            self.outbox.push_back(ServerEvent::Response {
                                request_id: request.id,
                                result: Box::new(ResponseResult::WorkspaceInfo {
                                    workspace: crate::api::schema::WorkspaceInfo {
                                        workspace_id: target.workspace_id.clone(),
                                        number: 1,
                                        label: target.workspace_id.clone(),
                                        focused: true,
                                        pane_count: 1,
                                        tab_count: 1,
                                        active_tab_id: format!("{}:tab", target.workspace_id),
                                        agent_status: crate::api::schema::AgentStatus::Unknown,
                                        tokens: Default::default(),
                                        worktree: None,
                                    },
                                }),
                            });
                            self.publish();
                        }
                        method => panic!("fake server does not handle {method:?}"),
                    }
                }
                ClientMessage::EndpointControl { kind, data }
                    if kind == crate::protocol::endpoint::PRESENTATION_EFFECTS_SYNC_KIND =>
                {
                    self.outbox.push_back(ServerEvent::EffectsReady(data));
                }
                _ => {}
            }
        }
    }

    fn workspace_focus_requests(&self) -> Vec<String> {
        self.inbox
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellEndpointRequest { request, .. } => {
                    match serde_json::from_str::<Request>(request).ok()?.method {
                        Method::WorkspaceFocus(target) => Some(target.workspace_id),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }
}

struct Harness {
    state: ClientState,
    endpoints: EndpointRegistry,
    commands: EndpointCommands,
    pending: Option<PendingEndpointActivation>,
    serial: u64,
    scheduled: Option<ClientLoopEvent>,
    prefix: crate::platform::RealPrefixInputSource,
    local: FakeServer,
    remote: FakeServer,
}

impl Harness {
    /// Local is presented with its last workspace focused. The saved machine is online with
    /// its last workspace focused, but does not hold the surface.
    fn new() -> Self {
        let profile = remote_profile();
        let remote_id = ClientEndpointId::Ssh(profile.id.clone());
        let mut state = ClientState::test_new();
        let mut shell = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        shell.set_endpoint_catalog(&[profile]);
        let geometry = shell.surface_size(state.reported_size.0, state.reported_size.1);
        let local = FakeServer::new(
            ClientEndpointId::Local,
            1,
            "local-boot",
            &["local_1", "local_2"],
            "local_2",
            true,
            geometry,
        );
        let remote = FakeServer::new(
            remote_id.clone(),
            7,
            "remote-boot",
            &["remote_1", "remote_2", "remote_3"],
            "remote_3",
            false,
            geometry,
        );
        shell.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
        shell.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
        shell.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            1,
            Box::new(local.snapshot()),
        );
        shell.set_pane_surface(local.surface());
        shell.cache_endpoint_snapshot_inactive_for_generation(
            &remote_id,
            7,
            Box::new(remote.snapshot()),
        );
        state.shell = Some(shell);

        let mut endpoints = EndpointRegistry::new(
            RecordingTransport(local.inbox.clone()),
            local.generation,
            negotiation(),
        );
        endpoints.insert(
            remote_id,
            RecordingTransport(remote.inbox.clone()),
            remote.generation,
            negotiation(),
            false,
        );
        Self {
            state,
            endpoints,
            commands: EndpointCommands::default(),
            pending: None,
            serial: 100,
            scheduled: None,
            prefix: crate::platform::RealPrefixInputSource::default(),
            local,
            remote,
        }
    }

    fn shell(&self) -> &ClientShellState {
        self.state.shell.as_ref().expect("client shell")
    }

    fn remote_id(&self) -> ClientEndpointId {
        self.remote.endpoint_id.clone()
    }

    fn handoff_in_progress(&self) -> bool {
        self.pending.is_some()
    }

    /// The user presses the `next_workspace` binding.
    fn press_next_workspace(&mut self) {
        let mut outcome = ClientShellInput::default();
        self.state
            .shell
            .as_mut()
            .expect("client shell")
            .record_binding(
                crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextWorkspace),
                &mut outcome,
            );
        finish_client_shell_input(
            &mut self.state,
            outcome,
            None,
            &mut self.endpoints,
            &mut self.pending,
            &mut self.commands,
            &mut self.prefix,
            &mut self.scheduled,
        )
        .unwrap();
        self.run_scheduled();
    }

    fn run_scheduled(&mut self) {
        while let Some(event) = self.scheduled.take() {
            let ClientLoopEvent::ActivateEndpoint {
                endpoint_id,
                target,
                force,
            } = event
            else {
                panic!("unexpected scheduled client loop event");
            };
            begin_endpoint_activation(
                &mut self.state,
                &mut self.endpoints,
                &mut self.commands,
                &mut self.pending,
                &mut self.serial,
                endpoint_id,
                target,
                force,
                std::time::Instant::now(),
                &mut self.scheduled,
            )
            .unwrap();
        }
    }

    fn complete_activation(&mut self) {
        if let Some(event) = complete_endpoint_activation(
            &mut self.state,
            &mut self.endpoints,
            &mut self.pending,
            &mut self.commands,
        )
        .unwrap()
        {
            self.scheduled = Some(event);
        }
    }

    /// Deliver one server event through the same routing as the client event loop.
    fn deliver(&mut self, endpoint_id: &ClientEndpointId, generation: u64, event: ServerEvent) {
        let endpoint_active = self.endpoints.active_id() == endpoint_id
            && self
                .endpoints
                .connection(endpoint_id)
                .is_some_and(|connection| connection.surface_active);
        let activation_message = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.accepts_endpoint(endpoint_id, generation));
        let boot_id = if endpoint_id.is_local() {
            self.local.boot_id.clone()
        } else {
            self.remote.boot_id.clone()
        };
        match event {
            ServerEvent::Response { request_id, result } => {
                let command_response =
                    self.commands
                        .accepts_response(endpoint_id, generation, &boot_id, &request_id);
                let wire = ServerMessage::ClientShellEndpointResponseChunk {
                    boot_id: boot_id.clone(),
                    request_id: request_id.clone(),
                    final_chunk: true,
                    data: Vec::new(),
                };
                if !accepts_endpoint_message(
                    endpoint_active,
                    activation_message,
                    command_response,
                    &wire,
                ) {
                    return;
                }
                let data = serde_json::to_vec(&SuccessResponse {
                    id: request_id.clone(),
                    result: *result,
                })
                .unwrap();
                if self.pending.as_ref().is_some_and(|pending| {
                    pending.accepts_response(endpoint_id, generation, &boot_id, &request_id)
                }) {
                    let progress = self.pending.as_mut().map(|pending| {
                        pending.receive_response_for_boot(
                            endpoint_id,
                            generation,
                            &boot_id,
                            &request_id,
                            &data,
                            &mut self.endpoints,
                        )
                    });
                    match progress {
                        Some(SurfaceActivationProgress::Ready) => self.complete_activation(),
                        Some(SurfaceActivationProgress::Rejected {
                            message,
                            source_release_rejected,
                        }) => rollback_endpoint_activation(
                            &mut self.state,
                            &mut self.endpoints,
                            &mut self.pending,
                            message,
                            source_release_rejected,
                        ),
                        _ => {}
                    }
                    return;
                }
                if request_id.starts_with("client-shell-surface:") {
                    return;
                }
                let Some(completed) = self
                    .commands
                    .receive_chunk(endpoint_id, generation, &boot_id, &request_id, true, data)
                    .unwrap()
                else {
                    return;
                };
                let shell = self.state.shell.as_mut().expect("client shell");
                let actions = if completed.generation == generation
                    && shell.endpoint_is_active(&completed.endpoint_id)
                {
                    shell
                        .handle_endpoint_result(
                            &completed.boot_id,
                            &completed.request_id,
                            completed.result,
                        )
                        .1
                } else {
                    shell.cancel_endpoint_request(&completed.request_id);
                    Vec::new()
                };
                dispatch_client_shell_actions(
                    actions,
                    &mut self.commands,
                    &mut self.endpoints,
                    self.state.shell.as_mut(),
                    &mut self.state.detached_process_children,
                    &mut self.scheduled,
                )
                .unwrap();
            }
            ServerEvent::Snapshot(snapshot) => {
                let progress = activation_message
                    .then(|| {
                        self.pending.as_mut().map(|pending| {
                            pending.receive_snapshot(endpoint_id, generation, &snapshot)
                        })
                    })
                    .flatten();
                install_client_shell_snapshot(
                    &mut self.state,
                    endpoint_id,
                    snapshot,
                    activation_message,
                    &mut self.endpoints,
                    &mut self.prefix,
                )
                .unwrap();
                if matches!(progress, Some(SurfaceActivationProgress::Ready)) {
                    self.complete_activation();
                }
            }
            ServerEvent::Surface(surface) => {
                if activation_message {
                    let progress = self
                        .pending
                        .as_mut()
                        .map(|pending| pending.receive_surface(endpoint_id, generation, *surface));
                    if matches!(progress, Some(SurfaceActivationProgress::Ready)) {
                        self.complete_activation();
                    }
                    return;
                }
                if endpoint_active {
                    self.state
                        .shell
                        .as_mut()
                        .expect("client shell")
                        .set_pane_surface(*surface);
                }
            }
            ServerEvent::EffectsReady(token) => {
                let progress = self.pending.as_mut().map(|pending| {
                    pending.receive_presentation_effects_ready(endpoint_id, generation, &token)
                });
                if matches!(progress, Some(SurfaceActivationProgress::Ready)) {
                    self.complete_activation();
                }
            }
        }
    }

    /// Deliver at most one server event. Returns false once both servers are idle.
    fn step(&mut self) -> bool {
        self.local.pump();
        self.remote.pump();
        let next = if let Some(event) = self.local.outbox.pop_front() {
            Some((ClientEndpointId::Local, self.local.generation, event))
        } else {
            self.remote.outbox.pop_front().map(|event| {
                (
                    self.remote.endpoint_id.clone(),
                    self.remote.generation,
                    event,
                )
            })
        };
        let Some((endpoint_id, generation, event)) = next else {
            return false;
        };
        self.deliver(&endpoint_id, generation, event);
        self.run_scheduled();
        true
    }

    fn run_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        while !done(self) {
            assert!(self.step(), "servers went idle before {what}");
        }
    }

    fn settle(&mut self) {
        while self.step() {}
        assert!(!self.handoff_in_progress(), "handoff did not finish");
    }

    fn interrupted_notice_visible(&self) -> bool {
        self.shell()
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.title == "Action interrupted")
    }
}

/// Sanity check for the harness: one press from Local's last workspace moves to the saved
/// machine's first workspace.
#[test]
fn next_workspace_crosses_from_local_to_saved_machine() {
    let mut h = Harness::new();
    h.press_next_workspace();
    assert!(h.handoff_in_progress());
    h.settle();

    assert!(h.shell().endpoint_is_active(&h.remote_id()));
    assert_eq!(h.endpoints.active_id(), &h.remote_id());
    assert!(h.endpoints.active_surface_available());
    assert_eq!(h.remote.focused, "remote_1");
    assert!(!h.interrupted_notice_visible());
}

/// Reproducer, late handoff phase. After the saved machine is projected (the sidebar shows it
/// as current) but before the handoff finishes, the next press targets another workspace on
/// that machine. It is cancelled with "Action interrupted" and never reaches the server.
#[test]
fn next_workspace_pressed_while_handoff_finishes_is_not_lost() {
    let mut h = Harness::new();
    h.press_next_workspace();
    let remote = h.remote_id();
    h.run_until("the saved machine is projected", |h| {
        h.shell().endpoint_is_active(&remote)
    });
    assert!(
        h.handoff_in_progress(),
        "precondition: the handoff is still synchronizing presentation"
    );

    h.press_next_workspace();
    h.settle();

    assert_eq!(
        h.remote.workspace_focus_requests(),
        vec!["remote_1".to_owned(), "remote_2".to_owned()],
        "the second press never reached the saved machine"
    );
    assert_eq!(
        h.remote.focused, "remote_2",
        "two presses from Local's last workspace should end on the saved machine's second workspace"
    );
    assert!(
        !h.interrupted_notice_visible(),
        "the second press was cancelled with \"Action interrupted\""
    );
}

/// Reproducer, early handoff phase. While the saved machine is still activating, the client
/// still shows Local's snapshot, so the next press resolves to the same first workspace on the
/// saved machine again and the press has no effect.
#[test]
fn next_workspace_pressed_while_target_activates_is_not_lost() {
    let mut h = Harness::new();
    h.press_next_workspace();
    let remote = h.remote_id();
    h.run_until("the saved machine receives surface activation", |h| {
        h.remote.surface_active
    });
    assert!(
        h.handoff_in_progress() && !h.shell().endpoint_is_active(&remote),
        "precondition: the saved machine is activating but not yet projected"
    );

    h.press_next_workspace();
    h.settle();

    assert!(h.shell().endpoint_is_active(&remote));
    assert_eq!(
        h.remote.focused, "remote_2",
        "two presses from Local's last workspace should end on the saved machine's second workspace; focus requests sent: {:?}",
        h.remote.workspace_focus_requests()
    );
}

/// Moves to the saved machine's last workspace, the start for the reverse direction.
fn harness_on_remote_last_workspace() -> Harness {
    let mut h = Harness::new();
    for expected in ["remote_1", "remote_2", "remote_3"] {
        h.press_next_workspace();
        h.settle();
        assert_eq!(h.remote.focused, expected);
    }
    assert!(h.shell().endpoint_is_active(&h.remote_id()));
    assert!(!h.local.surface_active);
    h
}

/// Reverse direction, early handoff phase: the client still shows the saved machine while
/// Local activates, so the next press resolves to Local's first workspace again.
#[test]
fn next_workspace_pressed_while_local_activates_is_not_lost() {
    let mut h = harness_on_remote_last_workspace();
    h.press_next_workspace();
    h.run_until("Local receives surface activation", |h| {
        h.local.surface_active
    });
    assert!(
        h.handoff_in_progress() && !h.shell().endpoint_is_active(&ClientEndpointId::Local),
        "precondition: Local is activating but not yet projected"
    );

    h.press_next_workspace();
    h.settle();

    assert!(h.shell().endpoint_is_active(&ClientEndpointId::Local));
    assert_eq!(
        h.local.focused, "local_2",
        "two presses from the saved machine's last workspace should end on Local's second workspace; focus requests sent: {:?}",
        h.local.workspace_focus_requests()
    );
}

/// Reverse direction, late handoff phase: Local is projected but the handoff is still
/// synchronizing presentation when the next press arrives.
#[test]
fn next_workspace_pressed_while_local_handoff_finishes_is_not_lost() {
    let mut h = harness_on_remote_last_workspace();
    h.press_next_workspace();
    h.run_until("Local is projected", |h| {
        h.shell().endpoint_is_active(&ClientEndpointId::Local)
    });
    assert!(
        h.handoff_in_progress(),
        "precondition: the handoff is still synchronizing presentation"
    );

    h.press_next_workspace();
    h.settle();

    assert_eq!(
        h.local.focused, "local_2",
        "two presses from the saved machine's last workspace should end on Local's second workspace; focus requests sent: {:?}",
        h.local.workspace_focus_requests()
    );
    assert!(!h.interrupted_notice_visible());
}
