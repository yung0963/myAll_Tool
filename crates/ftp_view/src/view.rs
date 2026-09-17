//! FTP/FTPS file browser tab.
//!
//! The SFTP view has a large amount of SSH-specific interaction state.  FTP
//! does not have those requirements, so this view keeps a small, explicit
//! state machine around the shared FTP backend and presents the same useful
//! two-pane workflow: local files on the left, the connected server on the
//! right.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use chrono::{DateTime, Local};
use ftp::{FtpClient, FtpConfig, FtpSecurity, FtpTransferMode, SuppaFtpClient};
use gpui::{
    App, ColorExt, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, IconSize, InteractiveElementExt, Sizable, WindowExt,
    button::{Button, ButtonVariants, IconButton, IconButtonRole},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    scroll::ScrollableElement,
    v_flex,
};
use one_core::gpui_tokio::Tokio;
use one_core::storage::models::{
    FtpParams, FtpSecurity as CoreFtpSecurity, FtpTransferMode as CoreFtpTransferMode,
    StoredConnection,
};
use one_core::tab_container::{TabConnectionStatus, TabContent, TabContentEvent};
use rust_i18n::t;
use tokio::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConnectionState {
    Connecting,
    Connected,
    Disconnected(Option<String>),
}

#[derive(Clone, Debug)]
struct FileRow {
    name: String,
    path: String,
    size: u64,
    is_dir: bool,
    modified: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameAction {
    Mkdir,
    Rename,
}

pub struct FtpView {
    config: FtpConfig,
    client: Option<Arc<Mutex<SuppaFtpClient>>>,
    connection_state: ConnectionState,
    connection_generation: u64,
    busy: bool,
    remote_loading: bool,
    error_message: Option<String>,

    local_path: PathBuf,
    local_entries: Vec<FileRow>,
    remote_path: String,
    remote_entries: Vec<FileRow>,
    selected_local: Option<usize>,
    selected_remote: Option<usize>,

    local_path_input: Entity<InputState>,
    remote_path_input: Entity<InputState>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<gpui::Subscription>,
    connection_name: String,
    tab_index: Option<usize>,
}

impl FtpView {
    pub fn new(connection: StoredConnection, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with_index(connection, None, window, cx)
    }

    pub fn new_with_index(
        connection: StoredConnection,
        tab_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let params = connection.to_ftp_params().unwrap_or_default();
        let config = backend_config(&params);
        let local_path = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let local_path_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(local_path.to_string_lossy().to_string())
        });
        let remote_path_input = cx
            .new(|cx| InputState::new(window, cx).default_value(params.initial_directory.clone()));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &local_path_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_local_path(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &remote_path_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_remote_path(window, cx);
                }
            },
        ));

        let mut view = Self {
            config,
            client: None,
            connection_state: ConnectionState::Disconnected(None),
            connection_generation: 0,
            busy: false,
            remote_loading: false,
            error_message: None,
            local_path: local_path.clone(),
            local_entries: Vec::new(),
            remote_path: params.initial_directory,
            remote_entries: Vec::new(),
            selected_local: None,
            selected_remote: None,
            local_path_input,
            remote_path_input,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            connection_name: connection.name,
            tab_index,
        };

        view.refresh_local_entries();
        view.connect(cx);
        view
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.connection_generation = self.connection_generation.wrapping_add(1).max(1);
        let generation = self.connection_generation;
        let config = self.config.clone();
        self.connection_state = ConnectionState::Connecting;
        self.error_message = None;
        self.busy = true;
        self.remote_loading = true;
        cx.notify();

        let task = Tokio::spawn_result(cx, async move {
            let mut client = ftp::connect(config).await?;
            let path = client
                .realpath(".")
                .await
                .unwrap_or_else(|_| "/".to_string());
            let entries = client.list(&path).await?;
            Ok::<_, anyhow::Error>((client, path, entries))
        });

        cx.spawn(async move |this, cx| match task.await {
            Ok((client, path, entries)) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.remote_path = path;
                    this.remote_entries = entries.into_iter().map(remote_row).collect();
                    this.selected_remote = None;
                    this.client = Some(Arc::new(Mutex::new(client)));
                    this.connection_state = ConnectionState::Connected;
                    this.busy = false;
                    this.remote_loading = false;
                    this.error_message = None;
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.connection_state = ConnectionState::Disconnected(Some(message.clone()));
                    this.error_message = Some(message);
                    this.busy = false;
                    this.remote_loading = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn refresh_local_entries(&mut self) {
        self.local_entries = read_local_entries(&self.local_path);
        self.selected_local = None;
    }

    fn refresh_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_local_entries();
        let local_path = self.local_path.to_string_lossy().to_string();
        self.local_path_input.update(cx, |input, input_cx| {
            input.set_value(local_path, window, input_cx)
        });
        window.push_notification(
            Notification::success(t!("FtpView.local_refreshed").to_string()).autohide(true),
            cx,
        );
        cx.notify();
    }

    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        self.remote_loading = true;
        let path = self.remote_path.clone();
        let generation = self.connection_generation;
        let requested_path = path.clone();
        let task = Tokio::spawn_result(cx, async move {
            let mut client = client.lock().await;
            client.list(&requested_path).await
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(entries) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.remote_entries = entries.into_iter().map(remote_row).collect();
                    this.selected_remote = None;
                    this.busy = false;
                    this.remote_loading = false;
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.remote_loading = false;
                    this.error_message = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn navigate_remote(&mut self, path: String, cx: &mut Context<Self>) {
        if self.busy || self.remote_path == path {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.busy = true;
        self.remote_loading = true;
        let generation = self.connection_generation;
        let requested_path = path.clone();
        let task = Tokio::spawn_result(cx, async move {
            let mut client = client.lock().await;
            client.list(&requested_path).await
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(entries) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.remote_path = path.clone();
                    this.remote_entries = entries.into_iter().map(remote_row).collect();
                    this.selected_remote = None;
                    this.busy = false;
                    this.remote_loading = false;
                    this.error_message = None;
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.remote_loading = false;
                    this.error_message = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn navigate_local(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !path.is_dir() {
            return;
        }
        self.local_path = path;
        self.refresh_local(window, cx);
    }

    fn confirm_local_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.local_path_input.read(cx).text().to_string();
        let path = path.trim();
        if !path.is_empty() {
            self.navigate_local(PathBuf::from(path), window, cx);
        }
    }

    fn confirm_remote_path(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let path = self.remote_path_input.read(cx).text().to_string();
        let path = path.trim();
        if !path.is_empty() && path.starts_with('/') {
            self.navigate_remote(path.to_string(), cx);
        }
    }

    fn go_local_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.local_path.parent() {
            self.navigate_local(parent.to_path_buf(), window, cx);
        }
    }

    fn go_remote_parent(&mut self, cx: &mut Context<Self>) {
        if self.remote_path == "/" {
            return;
        }
        let path = remote_parent(&self.remote_path);
        self.navigate_remote(path, cx);
    }

    fn upload_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_local else {
            notify_warning(window, t!("FtpView.select_local").to_string(), cx);
            return;
        };
        let Some(item) = self.local_entries.get(index).cloned() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let remote_path = join_remote(&self.remote_path, &item.name);
        self.busy = true;
        let generation = self.connection_generation;
        let local_path = item.path.clone();
        let is_dir = item.is_dir;
        let task = Tokio::spawn_result(cx, async move {
            let mut client = client.lock().await;
            if is_dir {
                client
                    .upload_dir_with_progress(
                        &local_path,
                        &remote_path,
                        ftp::DirectoryConflictPolicy::Merge,
                        Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        Box::new(|_| {}),
                    )
                    .await
            } else {
                client.upload(&local_path, &remote_path).await
            }
            .map_err(|error| anyhow!(error))
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(()) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.refresh_remote(cx);
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.error_message = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn download_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_remote else {
            notify_warning(window, t!("FtpView.select_remote").to_string(), cx);
            return;
        };
        let Some(item) = self.remote_entries.get(index).cloned() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        let local_path = self.local_path.join(&item.name);
        let local_path_string = local_path.to_string_lossy().to_string();
        let remote_path = item.path.clone();
        let is_dir = item.is_dir;
        self.busy = true;
        let generation = self.connection_generation;
        let task = Tokio::spawn_result(cx, async move {
            let mut client = client.lock().await;
            if is_dir {
                client
                    .download_dir_with_progress(
                        &remote_path,
                        &local_path_string,
                        Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        Box::new(|_| {}),
                    )
                    .await
            } else {
                client.download(&remote_path, &local_path_string).await
            }
            .map_err(|error| anyhow!(error))
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(()) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.refresh_local_entries();
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.error_message = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn show_name_dialog(
        &mut self,
        action: NameAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action == NameAction::Rename && self.selected_remote.is_none() {
            notify_warning(window, t!("FtpView.select_remote").to_string(), cx);
            return;
        }
        let default = if action == NameAction::Rename {
            self.selected_remote
                .and_then(|index| self.remote_entries.get(index))
                .map(|entry| entry.name.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(default));
        let view = cx.entity();
        let title = if action == NameAction::Rename {
            t!("FtpView.rename").to_string()
        } else {
            t!("FtpView.new_folder").to_string()
        };
        window.open_dialog(cx, move |dialog, _, _| {
            let input_for_ok = input.clone();
            let view_for_ok = view.clone();
            dialog
                .title(title.clone())
                .child(v_flex().gap_2().child(Input::new(&input)))
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(t!("Common.ok").to_string())
                        .cancel_text(t!("Common.cancel").to_string()),
                )
                .on_ok(move |_, window, cx| {
                    let name = input_for_ok.read(cx).text().to_string();
                    let name = name.trim().to_string();
                    if !valid_entry_name(&name) {
                        notify_warning(window, t!("FtpView.invalid_name").to_string(), cx);
                        return false;
                    }
                    view_for_ok.update(cx, |view, cx| {
                        view.submit_name_action(action, name, cx);
                    });
                    window.close_dialog(cx);
                    false
                })
        });
    }

    fn submit_name_action(&mut self, action: NameAction, name: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let target = join_remote(&self.remote_path, &name);
        let old = if action == NameAction::Rename {
            self.selected_remote
                .and_then(|index| self.remote_entries.get(index))
                .map(|entry| entry.path.clone())
        } else {
            None
        };
        self.busy = true;
        let generation = self.connection_generation;
        let task = Tokio::spawn_result(cx, async move {
            let mut client = client.lock().await;
            match (action, old) {
                (NameAction::Mkdir, _) => client.mkdir(&target).await,
                (NameAction::Rename, Some(old)) => client.rename(&old, &target).await,
                (NameAction::Rename, None) => Err(anyhow!("no remote entry selected")),
            }
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(()) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.refresh_remote(cx);
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.error_message = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_remote else {
            notify_warning(window, t!("FtpView.select_remote").to_string(), cx);
            return;
        };
        let Some(item) = self.remote_entries.get(index).cloned() else {
            return;
        };
        let client = self.client.clone();
        let view = cx.entity();
        let client_for_dialog = client.clone();
        let item_for_dialog = item.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let view_for_ok = view.clone();
            let client_for_ok = client_for_dialog.clone();
            let item_for_ok = item_for_dialog.clone();
            dialog
                .title(t!("FtpView.delete").to_string())
                .child(
                    div().child(
                        t!(
                            "FtpView.delete_confirm",
                            name = item_for_dialog.name.clone()
                        )
                        .to_string(),
                    ),
                )
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(t!("Common.ok").to_string())
                        .cancel_text(t!("Common.cancel").to_string()),
                )
                .on_ok(move |_, window, cx| {
                    if let Some(client) = client_for_ok.clone() {
                        view_for_ok.update(cx, |view, cx| {
                            view.start_delete(client, item_for_ok.clone(), cx);
                        });
                    }
                    window.close_dialog(cx);
                    false
                })
        });
    }

    fn start_delete(
        &mut self,
        client: Arc<Mutex<SuppaFtpClient>>,
        item: FileRow,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        let generation = self.connection_generation;
        let task = Tokio::spawn_result(cx, async move {
            let mut client = client.lock().await;
            if item.is_dir {
                client
                    .delete_recursive(
                        &item.path,
                        Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        Box::new(|_| {}),
                    )
                    .await
            } else {
                client.delete(&item.path, false).await
            }
        });
        cx.spawn(async move |this, cx| match task.await {
            Ok(()) => {
                let _ = this.update(cx, |this, cx| {
                    if this.connection_generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.refresh_remote(cx);
                    cx.notify();
                });
            }
            Err(error) => {
                let message = safe_error(&error);
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.error_message = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let connected = matches!(self.connection_state, ConnectionState::Connected);
        let has_local = self.selected_local.is_some();
        let has_remote = self.selected_remote.is_some();
        h_flex()
            .h(px(48.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Icon::new(IconName::Server)
                    .with_size(IconSize::Medium)
                    .text_color(cx.theme().link),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_base()
                    .text_ellipsis()
                    .child(self.connection_name.clone()),
            )
            .when(connected, |this| {
                this.child(
                    div()
                        .h(px(28.))
                        .px_2()
                        .gap_1()
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .bg(cx.theme().success.opacity(0.12))
                        .text_sm()
                        .text_color(cx.theme().success)
                        .child("●")
                        .child(t!("FtpView.connected").to_string()),
                )
            })
            .when(!connected, |this| {
                this.child(
                    Button::new("ftp-connect")
                        .icon(IconName::StatusConnected)
                        .label(t!("FtpView.reconnect").to_string())
                        .small()
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.connect(cx))),
                )
            })
            .child(
                Button::new("ftp-upload")
                    .icon(IconName::ArrowUp)
                    .label(t!("FtpView.upload").to_string())
                    .small()
                    .disabled(!connected || !has_local || self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.upload_selected(window, cx))),
            )
            .child(
                Button::new("ftp-download")
                    .icon(IconName::ArrowDown)
                    .label(t!("FtpView.download").to_string())
                    .small()
                    .disabled(!connected || !has_remote || self.busy)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.download_selected(window, cx)),
                    ),
            )
            .when_some(self.error_message.clone(), |this, error| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .text_ellipsis()
                        .child(error),
                )
            })
            .into_any_element()
    }

    fn render_local_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(panel_header(
                "ftp-local-header",
                IconName::HardDrive,
                t!("FtpView.local_files").to_string(),
                self.local_path_input.clone(),
                cx,
                cx.listener(|this, _, window, cx| this.go_local_parent(window, cx)),
                cx.listener(|this, _, window, cx| this.refresh_local(window, cx)),
            ))
            .child(self.render_column_header(cx))
            .child(self.render_file_rows(false, cx))
            .into_any_element()
    }

    fn render_remote_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let connected = matches!(self.connection_state, ConnectionState::Connected);
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(panel_header(
                "ftp-remote-header",
                IconName::Server,
                t!("FtpView.remote_files").to_string(),
                self.remote_path_input.clone(),
                cx,
                cx.listener(|this, _, _window, cx| this.go_remote_parent(cx)),
                cx.listener(|this, _, _window, cx| this.refresh_remote(cx)),
            ))
            .child(
                h_flex()
                    .h(px(34.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("ftp-new-folder")
                            .icon(IconName::NewFolder)
                            .ghost()
                            .small()
                            .compact()
                            .label(t!("FtpView.new_folder").to_string())
                            .disabled(!connected || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_name_dialog(NameAction::Mkdir, window, cx)
                            })),
                    )
                    .child(
                        Button::new("ftp-rename")
                            .icon(IconName::Edit)
                            .ghost()
                            .small()
                            .compact()
                            .label(t!("FtpView.rename").to_string())
                            .disabled(!connected || self.selected_remote.is_none() || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_name_dialog(NameAction::Rename, window, cx)
                            })),
                    )
                    .child(
                        Button::new("ftp-delete")
                            .icon(IconName::Remove)
                            .ghost()
                            .small()
                            .compact()
                            .label(t!("FtpView.delete").to_string())
                            .disabled(!connected || self.selected_remote.is_none() || self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.delete_selected(window, cx)),
                            ),
                    ),
            )
            .child(self.render_column_header(cx))
            .child(self.render_file_rows(true, cx))
            .into_any_element()
    }

    fn render_file_rows(&self, remote: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entries = if remote {
            &self.remote_entries
        } else {
            &self.local_entries
        };
        let selected = if remote {
            self.selected_remote
        } else {
            self.selected_local
        };
        let view = cx.entity();
        let border_color = cx.theme().border;
        let list_active = cx.theme().list_active;
        let list_hover = cx.theme().list_hover;
        let link_color = cx.theme().link;
        let muted_foreground = cx.theme().muted_foreground;
        let rows = entries.iter().enumerate().map(move |(index, item)| {
            let item_path = item.path.clone();
            let is_dir = item.is_dir;
            let name = item.name.clone();
            let selected_row = selected == Some(index);
            let display_name = if is_dir {
                format!("▸ {name}")
            } else {
                name.clone()
            };
            let size = if is_dir {
                String::new()
            } else {
                format_size(item.size)
            };
            let modified = format_modified(item.modified);
            let view_for_click = view.clone();
            let view_for_double_click = view.clone();
            div()
                .id(format!(
                    "ftp-{}-row-{}",
                    if remote { "remote" } else { "local" },
                    index
                ))
                .h(px(42.))
                .px_2()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(border_color)
                .when(selected_row, |this| this.bg(list_active))
                .hover(|this| this.bg(list_hover))
                .on_click(move |_, _, cx| {
                    view_for_click.update(cx, |this, cx| {
                        if remote {
                            this.selected_remote = Some(index);
                            this.selected_local = None;
                        } else {
                            this.selected_local = Some(index);
                            this.selected_remote = None;
                        }
                        cx.notify();
                    });
                })
                .on_double_click(move |_, window, cx| {
                    if !is_dir {
                        return;
                    }
                    view_for_double_click.update(cx, |this, cx| {
                        if remote {
                            this.navigate_remote(item_path.clone(), cx);
                        } else {
                            this.navigate_local(PathBuf::from(item_path.clone()), window, cx);
                        }
                    });
                })
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .overflow_hidden()
                        .child(
                            Icon::new(if is_dir {
                                IconName::Folder1
                            } else {
                                IconName::File
                            })
                            .with_size(IconSize::Small)
                            .flex_shrink_0()
                            .text_color(if is_dir {
                                link_color
                            } else {
                                muted_foreground
                            }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(display_name),
                        ),
                )
                .child(
                    div()
                        .w(px(92.))
                        .min_w_0()
                        .overflow_hidden()
                        .px_2()
                        .text_sm()
                        .text_color(muted_foreground)
                        .text_right()
                        .whitespace_nowrap()
                        .child(size),
                )
                .child(
                    div()
                        .w(px(150.))
                        .min_w_0()
                        .overflow_hidden()
                        .px_2()
                        .text_sm()
                        .text_color(muted_foreground)
                        .text_right()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(modified),
                )
        });
        let empty = if remote && !matches!(self.connection_state, ConnectionState::Connected) {
            t!("FtpView.not_connected").to_string()
        } else if self.remote_loading && remote {
            t!("FtpView.loading").to_string()
        } else if entries.is_empty() {
            t!("FtpView.empty").to_string()
        } else {
            String::new()
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scrollbar()
            .when(!empty.is_empty(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_4()
                        .text_color(muted_foreground)
                        .child(empty.clone()),
                )
            })
            .children(rows)
            .into_any_element()
    }

    fn render_column_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(30.))
            .px_2()
            .gap_2()
            .items_center()
            .bg(cx.theme().muted.opacity(0.08))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("FtpView.name").to_string()),
            )
            .child(
                div()
                    .w(px(92.))
                    .px_2()
                    .text_xs()
                    .text_right()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("FtpView.size").to_string()),
            )
            .child(
                div()
                    .w(px(150.))
                    .px_2()
                    .text_xs()
                    .text_right()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("FtpView.modified").to_string()),
            )
    }
}

impl Focusable for FtpView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<TabContentEvent> for FtpView {}

impl TabContent for FtpView {
    fn content_key(&self) -> &'static str {
        "FTP"
    }

    fn title(&self, _cx: &App) -> SharedString {
        match self.tab_index {
            Some(index) => format!("{}({index})", self.connection_name).into(),
            None => self.connection_name.clone().into(),
        }
    }

    fn icon(&self, _cx: &App) -> Option<Icon> {
        Some(
            Icon::new(IconName::Server)
                .with_size(IconSize::Default)
                .color(),
        )
    }

    fn is_disconnected(&self, _cx: &App) -> bool {
        matches!(self.connection_state, ConnectionState::Disconnected(_))
    }

    fn connection_status(&self, _cx: &App) -> Option<TabConnectionStatus> {
        Some(match self.connection_state {
            ConnectionState::Connecting => TabConnectionStatus::Connecting,
            ConnectionState::Connected => TabConnectionStatus::Connected,
            ConnectionState::Disconnected(_) => TabConnectionStatus::Disconnected,
        })
    }
}

impl Render for FtpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        let local = self.render_local_panel(cx);
        let remote = self.render_remote_panel(cx);
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(local)
                    .child(remote),
            )
    }
}

fn backend_config(params: &FtpParams) -> FtpConfig {
    FtpConfig {
        host: params.host.clone(),
        port: params.port,
        username: params.username.clone(),
        password: params.password.clone(),
        initial_directory: if params.initial_directory.is_empty() {
            "/".to_string()
        } else {
            params.initial_directory.clone()
        },
        security: match params.security {
            CoreFtpSecurity::Plain => FtpSecurity::Plain,
            CoreFtpSecurity::ExplicitTls => FtpSecurity::ExplicitTls,
            CoreFtpSecurity::ImplicitTls => FtpSecurity::ImplicitTls,
        },
        transfer_mode: match params.transfer_mode {
            CoreFtpTransferMode::Passive => FtpTransferMode::Passive,
            CoreFtpTransferMode::Active => FtpTransferMode::Active,
        },
        accept_invalid_certs: params.accept_invalid_certs,
        timeout: Duration::from_secs(params.connect_timeout.max(1)),
    }
}

fn remote_row(entry: ftp::FileEntry) -> FileRow {
    FileRow {
        name: entry.name,
        path: entry.path,
        size: entry.size,
        is_dir: entry.is_dir,
        modified: entry.modified,
    }
}

fn read_local_entries(path: &Path) -> Vec<FileRow> {
    let mut entries = std::fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            Some(FileRow {
                path: entry.path().to_string_lossy().to_string(),
                name,
                size: metadata.len(),
                is_dir: metadata.is_dir(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    entries
}

fn panel_header(
    id: &'static str,
    icon: IconName,
    title: String,
    path_input: Entity<InputState>,
    cx: &mut Context<FtpView>,
    on_parent: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_refresh: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .h(px(44.))
        .px_3()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(Icon::new(icon).with_size(IconSize::Small))
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            IconButton::new(format!("{id}-parent"), IconName::ChevronLeft)
                .role(IconButtonRole::Toolbar)
                .tooltip(t!("FtpView.parent"))
                .on_click(on_parent),
        )
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .h(px(30.))
                .px_2()
                .rounded(px(6.))
                .bg(cx.theme().secondary)
                .child(
                    Input::new(&path_input)
                        .small()
                        .appearance(false)
                        .cleanable(false)
                        .w_full(),
                ),
        )
        .child(
            IconButton::new(format!("{id}-refresh"), IconName::Refresh)
                .role(IconButtonRole::Toolbar)
                .tooltip(t!("Common.refresh"))
                .on_click(on_refresh),
        )
}

fn join_remote(base: &str, name: &str) -> String {
    if base == "/" || base.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{}", base.trim_end_matches('/'), name)
    }
}

fn remote_parent(path: &str) -> String {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => path[..index].to_string(),
    }
}

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn format_size(size: u64) -> String {
    if size >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", size as f64 / (1024. * 1024. * 1024.))
    } else if size >= 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024. * 1024.))
    } else if size >= 1024 {
        format!("{:.1} KB", size as f64 / 1024.)
    } else {
        format!("{size} B")
    }
}

fn format_modified(time: SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

fn safe_error(error: &anyhow::Error) -> String {
    error.to_string().replace(['\r', '\n'], " ")
}

fn notify_warning(window: &mut Window, message: String, cx: &mut App) {
    window.push_notification(Notification::warning(message).autohide(true), cx);
}
