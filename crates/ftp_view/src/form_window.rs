use connection_form::credential::{
    CredentialCapabilities, CredentialPickerConfig, CredentialPickerEvent,
    CredentialReferencePicker, create_credential_picker, resolve_connection_for_runtime,
};
use connection_form::team::{
    TeamSelectItem, connection_sync_controls_visible_in, create_team_select, refresh_team_options,
    refresh_teams_tooltip, resolve_team_assignment, selected_team_id, team_label,
    team_management_enabled,
};
use ftp::{
    FtpConfig as BackendFtpConfig, FtpSecurity as BackendFtpSecurity,
    FtpTransferMode as BackendFtpTransferMode,
};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, AsyncApp, ColorExt, Context, Div, Entity, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme, Disableable, IconName, Sizable,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    radio::Radio,
    scroll::ScrollableElement,
    select::{Select, SelectItem, SelectState},
    v_flex,
};
use one_core::cloud_sync::TeamOption;
use one_core::connection_notifier::{ConnectionDataEvent, emit_connection_event};
use one_core::gpui_tokio::Tokio;
use one_core::storage::traits::Repository;
use one_core::storage::{FtpParams, FtpSecurity, FtpTransferMode, StoredConnection, Workspace};
use rust_i18n::t;
use std::sync::Arc;
use std::time::Duration;

/// Action requested after a connection form saves a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FtpFormPostSaveAction {
    Close,
    Continue,
}

/// Callback used by import/editor flows after the connection has been stored.
pub type FtpFormSavedCallback = Arc<
    dyn Fn(StoredConnection, FtpFormPostSaveAction, &mut Window, &mut App) + Send + Sync + 'static,
>;

/// Configuration for creating or editing an FTP/FTPS connection.
pub struct FtpFormWindowConfig {
    pub editing_connection: Option<StoredConnection>,
    pub initial_connection: Option<StoredConnection>,
    pub on_saved: Option<FtpFormSavedCallback>,
    pub workspaces: Vec<Workspace>,
    pub teams: Vec<TeamOption>,
}

impl FtpFormWindowConfig {
    pub fn is_editing(&self) -> bool {
        self.editing_connection.is_some()
    }

    pub fn supports_save_and_continue(&self) -> bool {
        self.on_saved.is_some()
    }

    fn connection_to_load(&self) -> Option<&StoredConnection> {
        self.editing_connection
            .as_ref()
            .or(self.initial_connection.as_ref())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveAction {
    Close,
    Continue,
}

fn post_save_action(action: SaveAction) -> FtpFormPostSaveAction {
    match action {
        SaveAction::Close => FtpFormPostSaveAction::Close,
        SaveAction::Continue => FtpFormPostSaveAction::Continue,
    }
}

#[derive(Clone, Default, PartialEq)]
struct WorkspaceSelectItem {
    id: Option<i64>,
    name: String,
}

impl WorkspaceSelectItem {
    fn none() -> Self {
        Self {
            id: None,
            name: t!("Common.none").to_string(),
        }
    }

    fn from_workspace(workspace: &Workspace) -> Self {
        Self {
            id: workspace.id,
            name: workspace.name.clone(),
        }
    }
}

impl SelectItem for WorkspaceSelectItem {
    type Value = Option<i64>;

    fn title(&self) -> SharedString {
        self.name.clone().into()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

fn security_label(security: FtpSecurity) -> String {
    match security {
        FtpSecurity::Plain => t!("FTP.plain").to_string(),
        FtpSecurity::ExplicitTls => t!("FTP.explicit_tls").to_string(),
        FtpSecurity::ImplicitTls => t!("FTP.implicit_tls").to_string(),
    }
}

fn transfer_mode_label(mode: FtpTransferMode) -> String {
    match mode {
        FtpTransferMode::Passive => t!("FTP.passive").to_string(),
        FtpTransferMode::Active => t!("FTP.active").to_string(),
    }
}

fn backend_security(security: FtpSecurity) -> BackendFtpSecurity {
    match security {
        FtpSecurity::Plain => BackendFtpSecurity::Plain,
        FtpSecurity::ExplicitTls => BackendFtpSecurity::ExplicitTls,
        FtpSecurity::ImplicitTls => BackendFtpSecurity::ImplicitTls,
    }
}

fn backend_transfer_mode(mode: FtpTransferMode) -> BackendFtpTransferMode {
    match mode {
        FtpTransferMode::Passive => BackendFtpTransferMode::Passive,
        FtpTransferMode::Active => BackendFtpTransferMode::Active,
    }
}

/// Validate the protocol fields before either testing or saving a connection.
///
/// The FTP backend currently exposes configuration validation but not a
/// client/session API. The form therefore uses this shared validation and a
/// TCP reachability check for its Test button; the result deliberately does
/// not claim that credentials or the FTP protocol handshake succeeded.
pub fn validate_ftp_params(params: &FtpParams) -> Result<(), String> {
    if params.connect_timeout == 0 {
        return Err("FTP connection timeout must be greater than zero".to_string());
    }

    let config = BackendFtpConfig {
        host: params.host.clone(),
        port: params.port,
        username: params.username.clone(),
        password: params.password.clone(),
        initial_directory: params.initial_directory.clone(),
        security: backend_security(params.security),
        transfer_mode: backend_transfer_mode(params.transfer_mode),
        accept_invalid_certs: params.accept_invalid_certs,
        timeout: Duration::from_secs(params.connect_timeout),
    };
    config.validate().map_err(|error| error.to_string())
}

/// A standalone FTP/FTPS connection form.
pub struct FtpFormWindow {
    focus_handle: FocusHandle,
    is_editing: bool,
    editing_id: Option<i64>,
    editing_cloud_id: Option<String>,
    editing_last_synced_at: Option<i64>,
    editing_owner_id: Option<String>,

    name_input: Entity<InputState>,
    host_input: Entity<InputState>,
    port_input: Entity<InputState>,
    username_input: Entity<InputState>,
    password_input: Entity<InputState>,
    credential_picker: Entity<CredentialReferencePicker>,
    initial_directory_input: Entity<InputState>,
    connect_timeout_input: Entity<InputState>,
    workspace_select: Entity<SelectState<Vec<WorkspaceSelectItem>>>,
    team_select: Entity<SelectState<Vec<TeamSelectItem>>>,
    remark_input: Entity<InputState>,

    security: FtpSecurity,
    transfer_mode: FtpTransferMode,
    accept_invalid_certs: bool,
    sync_enabled: bool,

    is_testing: bool,
    test_result: Option<Result<(), String>>,
    on_saved: Option<FtpFormSavedCallback>,
    save_action: SaveAction,
}

impl FtpFormWindow {
    pub fn new(config: FtpFormWindowConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let is_editing = config.is_editing();
        let on_saved = config.on_saved.clone();
        let editing_id = config.editing_connection.as_ref().and_then(|c| c.id);
        let editing_cloud_id = config
            .editing_connection
            .as_ref()
            .and_then(|c| c.cloud_id.clone());
        let editing_last_synced_at = config
            .editing_connection
            .as_ref()
            .and_then(|c| c.last_synced_at);
        let editing_owner_id = config
            .editing_connection
            .as_ref()
            .and_then(|c| c.owner_id.clone());

        let defaults = FtpParams::default();
        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("FTP.name_placeholder")));
        let host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("FTP.host_placeholder")));
        let port_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(t!("FTP.port_placeholder"));
            state.set_value(defaults.port.to_string(), window, cx);
            state
        });
        let username_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(t!("FTP.username_placeholder"));
            state.set_value(&defaults.username, window, cx);
            state
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("FTP.password_placeholder"))
                .masked(true)
        });
        password_input.update(cx, |state, cx| {
            state.set_value(&defaults.password, window, cx);
        });
        let initial_directory_input = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder(t!("FTP.initial_directory_placeholder"));
            state.set_value(&defaults.initial_directory, window, cx);
            state
        });
        let connect_timeout_input = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder(t!("FTP.connect_timeout_placeholder"));
            state.set_value(defaults.connect_timeout.to_string(), window, cx);
            state
        });
        let remark_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("FTP.remark_placeholder"))
                .auto_grow(3, 10)
        });

        let mut workspace_items = vec![WorkspaceSelectItem::none()];
        workspace_items.extend(
            config
                .workspaces
                .iter()
                .map(WorkspaceSelectItem::from_workspace),
        );
        let workspace_select =
            cx.new(|cx| SelectState::new(workspace_items, Some(Default::default()), window, cx));

        let mut sync_enabled = true;
        let mut workspace_id = None;
        let mut team_id = None;
        let mut security = defaults.security;
        let mut transfer_mode = defaults.transfer_mode;
        let mut accept_invalid_certs = defaults.accept_invalid_certs;
        let mut credential_reference = None;

        if let Some(connection) = config.connection_to_load() {
            sync_enabled = connection.sync_enabled;
            workspace_id = connection.workspace_id;
            team_id = connection.team_id.clone();

            name_input.update(cx, |state, cx| {
                state.set_value(&connection.name, window, cx);
            });
            if let Ok(params) = connection.to_ftp_params() {
                host_input.update(cx, |state, cx| {
                    state.set_value(&params.host, window, cx);
                });
                port_input.update(cx, |state, cx| {
                    state.set_value(params.port.to_string(), window, cx);
                });
                username_input.update(cx, |state, cx| {
                    state.set_value(&params.username, window, cx);
                });
                password_input.update(cx, |state, cx| {
                    state.set_value(&params.password, window, cx);
                });
                credential_reference = params.credential_reference.clone();
                initial_directory_input.update(cx, |state, cx| {
                    state.set_value(&params.initial_directory, window, cx);
                });
                connect_timeout_input.update(cx, |state, cx| {
                    state.set_value(params.connect_timeout.to_string(), window, cx);
                });
                security = params.security;
                transfer_mode = params.transfer_mode;
                accept_invalid_certs = params.accept_invalid_certs;
            }
            if let Some(remark) = connection.remark.as_ref() {
                remark_input.update(cx, |state, cx| {
                    state.set_value(remark, window, cx);
                });
            }
        }

        if let Some(workspace_id) = workspace_id {
            workspace_select.update(cx, |select, cx| {
                select.set_selected_value(&Some(workspace_id), window, cx);
            });
        }
        let team_select = create_team_select(&config.teams, team_id.as_deref(), window, cx);
        let credential_picker = create_credential_picker(
            CredentialPickerConfig::new("ftp-credential", CredentialCapabilities::login())
                .reference(credential_reference),
            window,
            cx,
        );
        cx.subscribe(&credential_picker, |_, _, _: &CredentialPickerEvent, cx| {
            cx.notify();
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            is_editing,
            editing_id,
            editing_cloud_id,
            editing_last_synced_at,
            editing_owner_id,
            name_input,
            host_input,
            port_input,
            username_input,
            password_input,
            credential_picker,
            initial_directory_input,
            connect_timeout_input,
            workspace_select,
            team_select,
            remark_input,
            security,
            transfer_mode,
            accept_invalid_certs,
            sync_enabled,
            is_testing: false,
            test_result: None,
            on_saved,
            save_action: SaveAction::Close,
        }
    }

    fn get_workspace_id(&self, cx: &App) -> Option<i64> {
        self.workspace_select
            .read(cx)
            .selected_value()
            .cloned()
            .flatten()
    }

    fn get_team_id(&self, cx: &App) -> Option<String> {
        selected_team_id(&self.team_select, cx)
    }

    fn request_team_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        refresh_team_options(&self.team_select, window, cx);
    }

    fn text(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).text().to_string()
    }

    fn parse_u16(input: &Entity<InputState>, cx: &App) -> Option<u16> {
        Self::text(input, cx).trim().parse::<u16>().ok()
    }

    fn parse_u64(input: &Entity<InputState>, cx: &App) -> Option<u64> {
        Self::text(input, cx).trim().parse::<u64>().ok()
    }

    fn build_ftp_params(&self, cx: &App) -> Result<FtpParams, String> {
        let initial_directory = {
            let value = Self::text(&self.initial_directory_input, cx);
            if value.trim().is_empty() {
                "/".to_string()
            } else {
                value.trim().to_string()
            }
        };
        let params = FtpParams {
            host: Self::text(&self.host_input, cx).trim().to_string(),
            port: Self::parse_u16(&self.port_input, cx).unwrap_or_default(),
            username: Self::text(&self.username_input, cx),
            password: Self::text(&self.password_input, cx),
            credential_reference: self.credential_picker.read(cx).selected_reference(),
            initial_directory,
            security: self.security,
            transfer_mode: self.transfer_mode,
            accept_invalid_certs: self.accept_invalid_certs,
            connect_timeout: Self::parse_u64(&self.connect_timeout_input, cx).unwrap_or_default(),
        };
        validate_ftp_params(&params).map(|_| params)
    }

    fn set_test_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        self.is_testing = false;
        self.test_result = Some(Err(error.into()));
        cx.notify();
    }

    fn on_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let params = match self.build_ftp_params(cx) {
            Ok(params) => params,
            Err(_) => {
                self.set_test_error(t!("FTP.validation_error"), cx);
                return;
            }
        };
        let test_connection =
            StoredConnection::new_ftp("FTP connection test".to_string(), params, None);
        let params =
            match resolve_connection_for_runtime(test_connection, cx).and_then(|connection| {
                connection
                    .to_ftp_params()
                    .map_err(|error| error.to_string())
            }) {
                Ok(params) => params,
                Err(error) => {
                    self.set_test_error(error, cx);
                    return;
                }
            };

        self.is_testing = true;
        self.test_result = None;
        cx.notify();

        let window_handle = window.window_handle();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = Tokio::spawn_result(cx, async move {
                tokio::time::timeout(
                    Duration::from_secs(params.connect_timeout),
                    tokio::net::TcpStream::connect((params.host.as_str(), params.port)),
                )
                .await
                .map_err(|_| anyhow::anyhow!(t!("FTP.test_timeout").to_string()))?
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                Ok::<(), anyhow::Error>(())
            })
            .await;

            let _ = cx.update_window(window_handle, |_, _window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.is_testing = false;
                    this.test_result = Some(match result {
                        Ok(()) => Ok(()),
                        Err(error) => Err(format!("{error:#}")),
                    });
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn on_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_action = SaveAction::Close;
        self.save(window, cx);
    }

    fn on_save_and_continue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_action = SaveAction::Continue;
        self.save(window, cx);
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_testing {
            self.set_test_error(t!("FTP.save_while_testing"), cx);
            return;
        }

        let params = match self.build_ftp_params(cx) {
            Ok(params) => params,
            Err(_) => {
                self.set_test_error(t!("FTP.validation_error"), cx);
                return;
            }
        };

        let workspace_id = self.get_workspace_id(cx);
        let name = Self::text(&self.name_input, cx);
        let mut connection = StoredConnection::new_ftp(name, params, workspace_id);
        connection.sync_enabled = self.sync_enabled;

        let assignment = match resolve_team_assignment(
            self.get_team_id(cx),
            self.is_editing,
            self.editing_owner_id.clone(),
            cx,
        ) {
            Ok(assignment) => assignment,
            Err(error) => {
                self.set_test_error(error.to_string(), cx);
                return;
            }
        };
        connection.team_id = assignment.team_id;
        connection.owner_id = assignment.owner_id;
        if self.is_editing {
            connection.id = self.editing_id;
            connection.cloud_id = self.editing_cloud_id.clone();
            connection.last_synced_at = self.editing_last_synced_at;
        }

        let remark = Self::text(&self.remark_input, cx);
        if !remark.trim().is_empty() {
            connection.remark = Some(remark);
        }

        let storage = cx
            .global::<one_core::storage::GlobalStorageState>()
            .storage
            .clone();
        let is_editing = self.is_editing;
        let result: Result<StoredConnection, anyhow::Error> = (|| {
            let repository = storage
                .get::<one_core::storage::ConnectionRepository>()
                .ok_or_else(|| anyhow::anyhow!("ConnectionRepository not found"))?;
            if is_editing {
                repository.update(&connection)?;
            } else {
                let mut connection = connection;
                repository.insert(&mut connection)?;
                return Ok(connection);
            }
            Ok(connection)
        })();

        match result {
            Ok(saved_connection) => {
                emit_connection_event(
                    if is_editing {
                        ConnectionDataEvent::ConnectionUpdated {
                            connection: saved_connection.clone(),
                        }
                    } else {
                        ConnectionDataEvent::ConnectionCreated {
                            connection: saved_connection.clone(),
                        }
                    },
                    cx,
                );
                if let Some(callback) = self.on_saved.as_ref() {
                    callback(
                        saved_connection,
                        post_save_action(self.save_action),
                        window,
                        cx,
                    );
                }
                window.remove_window();
            }
            Err(error) => {
                let message = t!("FTP.save_failed", error = error).to_string();
                tracing::error!("FTP connection save failed");
                self.set_test_error(message, cx);
            }
        }
    }

    fn on_cancel(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    fn render_form_row(&self, label: &str, child: impl IntoElement) -> Div {
        h_flex()
            .gap_4()
            .items_center()
            .child(
                div()
                    .w(px(190.0))
                    .flex_shrink_0()
                    .text_sm()
                    .text_right()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(label.to_string()),
            )
            .child(div().flex_1().min_w_0().child(child))
    }

    fn render_section_header(&self, label: impl Into<String>, cx: &mut Context<Self>) -> Div {
        h_flex()
            .gap_2()
            .items_center()
            .pt_2()
            .pb_1()
            .child(
                div()
                    .w(px(3.0))
                    .h(px(16.0))
                    .rounded(px(2.0))
                    .bg(cx.theme().primary),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(label.into()),
            )
    }

    fn render_security(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.security;
        self.render_form_row(
            &t!("FTP.security"),
            h_flex()
                .gap_4()
                .flex_wrap()
                .child(
                    Radio::new("ftp-security-plain")
                        .label(security_label(FtpSecurity::Plain))
                        .checked(selected == FtpSecurity::Plain)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.security = FtpSecurity::Plain;
                            cx.notify();
                        })),
                )
                .child(
                    Radio::new("ftp-security-explicit")
                        .label(security_label(FtpSecurity::ExplicitTls))
                        .checked(selected == FtpSecurity::ExplicitTls)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.security = FtpSecurity::ExplicitTls;
                            cx.notify();
                        })),
                )
                .child(
                    Radio::new("ftp-security-implicit")
                        .label(security_label(FtpSecurity::ImplicitTls))
                        .checked(selected == FtpSecurity::ImplicitTls)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.security = FtpSecurity::ImplicitTls;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_transfer_mode(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.transfer_mode;
        self.render_form_row(
            &t!("FTP.transfer_mode"),
            h_flex()
                .gap_4()
                .child(
                    Radio::new("ftp-transfer-passive")
                        .label(transfer_mode_label(FtpTransferMode::Passive))
                        .checked(selected == FtpTransferMode::Passive)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transfer_mode = FtpTransferMode::Passive;
                            cx.notify();
                        })),
                )
                .child(
                    Radio::new("ftp-transfer-active")
                        .label(transfer_mode_label(FtpTransferMode::Active))
                        .checked(selected == FtpTransferMode::Active)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transfer_mode = FtpTransferMode::Active;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_result(&self, cx: &mut Context<Self>) -> Option<Div> {
        self.test_result.as_ref().map(|result| {
            let (color, message) = match result {
                Ok(()) => (cx.theme().success, t!("FTP.test_tcp_success").to_string()),
                Err(error) => (cx.theme().danger, error.clone()),
            };
            div()
                .mx_4()
                .px_3()
                .py_2()
                .rounded(px(6.0))
                .bg(color.opacity(0.1))
                .text_sm()
                .text_color(color)
                .child(message)
        })
    }
}

impl Focusable for FtpFormWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FtpFormWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_testing = self.is_testing;

        v_flex()
            .size_full()
            .child(
                div().flex_1().min_h_0().p_4().overflow_y_scrollbar().child(
                    v_flex()
                        .gap_3()
                        .child(self.render_section_header(t!("FTP.connection_section"), cx))
                        .child(self.render_form_row(
                            &t!("FTP.name"),
                            Input::new(&self.name_input).w_full(),
                        ))
                        .child(self.render_form_row(
                            &t!("FTP.host"),
                            Input::new(&self.host_input).w_full(),
                        ))
                        .child(self.render_form_row(
                            &t!("FTP.port"),
                            div().w(px(140.0)).child(Input::new(&self.port_input)),
                        ))
                        .child(self.render_section_header(t!("FTP.authentication_section"), cx))
                        .child(self.render_form_row(
                            &t!("FTP.username"),
                            Input::new(&self.username_input).w_full(),
                        ))
                        .child(
                            self.render_form_row(
                                &t!("FTP.credential"),
                                self.credential_picker.clone(),
                            ),
                        )
                        .child(self.render_form_row(
                            &t!("FTP.password"),
                            Input::new(&self.password_input).mask_toggle().w_full(),
                        ))
                        .child(self.render_form_row(
                            &t!("FTP.initial_directory"),
                            Input::new(&self.initial_directory_input).w_full(),
                        ))
                        .child(self.render_section_header(t!("FTP.transfer_section"), cx))
                        .child(self.render_security(cx))
                        .child(self.render_transfer_mode(cx))
                        .child(
                            self.render_form_row(
                                &t!("FTP.accept_invalid_certs"),
                                h_flex()
                                    .gap_2()
                                    .items_start()
                                    .child(
                                        Checkbox::new("ftp-accept-invalid-certs")
                                            .checked(self.accept_invalid_certs)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.accept_invalid_certs =
                                                    !this.accept_invalid_certs;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(t!("FTP.accept_invalid_certs_hint").to_string()),
                                    ),
                            ),
                        )
                        .child(self.render_section_header(t!("FTP.advanced_section"), cx))
                        .child(
                            self.render_form_row(
                                &t!("FTP.connect_timeout"),
                                div()
                                    .w(px(140.0))
                                    .child(Input::new(&self.connect_timeout_input)),
                            ),
                        )
                        .child(self.render_form_row(
                            &t!("FTP.workspace"),
                            Select::new(&self.workspace_select).w_full(),
                        ))
                        .when(
                            connection_sync_controls_visible_in(cx) && team_management_enabled(cx),
                            |form| {
                                form.child(
                                    self.render_form_row(
                                        &team_label(),
                                        h_flex()
                                            .gap_2()
                                            .child(Select::new(&self.team_select).w_full())
                                            .child(
                                                Button::new("sync-ftp-teams")
                                                    .icon(IconName::Refresh)
                                                    .ghost()
                                                    .tooltip(refresh_teams_tooltip())
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.request_team_sync(window, cx);
                                                        },
                                                    )),
                                            ),
                                    ),
                                )
                            },
                        )
                        .when(connection_sync_controls_visible_in(cx), |form| {
                            form.child(
                                self.render_form_row(
                                    &t!("ConnectionForm.cloud_sync"),
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Checkbox::new("ftp-sync-enabled")
                                                .checked(self.sync_enabled)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.sync_enabled = !this.sync_enabled;
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(
                                                    t!("ConnectionForm.cloud_sync_desc")
                                                        .to_string(),
                                                ),
                                        ),
                                ),
                            )
                        })
                        .child(self.render_form_row(
                            &t!("FTP.remark"),
                            Input::new(&self.remark_input).w_full(),
                        )),
                ),
            )
            .when_some(self.render_result(cx), |this, result| {
                this.child(h_flex().justify_center().pb_2().child(result))
            })
            .child(
                h_flex()
                    .flex_shrink_0()
                    .justify_end()
                    .gap_2()
                    .px_6()
                    .py_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("ftp-cancel")
                            .small()
                            .label(t!("Common.cancel").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_cancel(window, cx);
                            })),
                    )
                    .child(
                        Button::new("ftp-test")
                            .small()
                            .outline()
                            .label(if is_testing {
                                t!("Connection.testing").to_string()
                            } else {
                                t!("FTP.test_tcp").to_string()
                            })
                            .disabled(is_testing)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_test(window, cx);
                            })),
                    )
                    .when(self.on_saved.is_some(), |this| {
                        this.child(
                            Button::new("ftp-save-continue")
                                .small()
                                .outline()
                                .label(t!("Common.save_and_continue").to_string())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_save_and_continue(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("ftp-ok")
                            .small()
                            .primary()
                            .label(t!("Common.ok").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_save(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use one_core::storage::ConnectionType;

    #[test]
    fn ftp_defaults_are_safe_and_validate_after_host_is_set() {
        let mut params = FtpParams::default();
        assert!(validate_ftp_params(&params).is_err());

        params.host = "ftp.example.test".to_string();
        assert!(validate_ftp_params(&params).is_ok());
    }

    #[test]
    fn validation_rejects_invalid_port_directory_and_timeout() {
        let mut params = FtpParams {
            host: "ftp.example.test".to_string(),
            ..Default::default()
        };

        params.port = 0;
        assert!(validate_ftp_params(&params).is_err());

        params.port = 21;
        params.initial_directory = "uploads".to_string();
        assert!(validate_ftp_params(&params).is_err());

        params.initial_directory = "/uploads".to_string();
        params.connect_timeout = 0;
        assert!(validate_ftp_params(&params).is_err());
    }

    #[test]
    fn stored_connection_round_trip_preserves_all_ftp_form_fields() {
        let params = FtpParams {
            host: "secure.example.test".to_string(),
            port: 990,
            username: "alice".to_string(),
            password: "secret".to_string(),
            credential_reference: None,
            initial_directory: "/incoming".to_string(),
            security: FtpSecurity::ImplicitTls,
            transfer_mode: FtpTransferMode::Active,
            accept_invalid_certs: true,
            connect_timeout: 45,
        };
        let connection = StoredConnection::new_ftp("Secure FTP".to_string(), params.clone(), None);
        assert_eq!(ConnectionType::Ftp, connection.connection_type);
        assert_eq!(
            params,
            connection.to_ftp_params().expect("FTP params parse")
        );
    }

    #[test]
    fn config_supports_prefill_and_save_and_continue() {
        let config = FtpFormWindowConfig {
            editing_connection: None,
            initial_connection: Some(StoredConnection::new_ftp(
                "Imported".to_string(),
                FtpParams::default(),
                None,
            )),
            on_saved: Some(Arc::new(|_, _, _, _| {})),
            workspaces: Vec::new(),
            teams: Vec::new(),
        };
        assert!(!config.is_editing());
        assert!(config.supports_save_and_continue());
        assert!(config.connection_to_load().is_some());
    }
}
