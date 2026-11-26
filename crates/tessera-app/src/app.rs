use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use gpui::AppContext;
use gpui::ClipboardItem;
use gpui::Context;
use gpui::Entity;
use gpui::Focusable;
use gpui::FontWeight;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::KeyDownEvent;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::Window;
use gpui::div;
use gpui::prelude::FluentBuilder;
use gpui::px;

use rfd::FileDialog;

use gpui::RenderImage;

use crate::AssetColumnFilters;
use crate::AssetPreview;
use crate::ControllerMessage;
use crate::SearchTerm;
use crate::app_state::{ActiveAssetDownload, ActiveDownload, AppState};
use crate::components::AssetAction;
use crate::components::AssetTableDelegate;
use crate::components::AssetTableRequest;
use crate::components::BindAnimationModelAction;
use crate::components::Button;
use crate::components::DataTable;
use crate::components::Dialog;
use crate::components::Disableable;
use crate::components::FilterListChanged;
use crate::components::FilterListState;
use crate::components::Icon;
use crate::components::IconName;
use crate::components::Input;
use crate::components::InputEvent;
use crate::components::InputState;
use crate::components::Notification;
use crate::components::PreviewPane;
use crate::components::Progress;
use crate::components::Sizable;
use crate::components::Spinner;
use crate::components::TableState;
use crate::components::WindowExt;
use crate::components::h_flex;
use crate::components::title_bar::{AppTitleBar, TitleBarEvent};
use crate::components::v_flex;
use crate::controller::DownloadError;
use crate::palette;
use crate::system;
use crate::theme::ActiveTesseraTheme;
use crate::update;
use crate::update::DownloadProgress;
use crate::update::UpdateError;
use crate::update::UpdateInfo;
use crate::update::UpdateKind;
use crate::update::UpdateOutcome;
use crate::update::UpdateRepository;

/// The maximum number of assets before search isn't realtime.
const SEARCH_REALTIME_MAX: usize = 500_000;

/// Events sent from update background threads to the ui.
enum UpdateEvent {
    /// An update is available for prompting.
    Available(UpdateInfo),
    /// A manual check found no updates.
    UpToDate,
    /// The user accepted the pending update prompt.
    Accepted,
    /// The user declined the pending update prompt.
    Declined,
    /// Download progress for the active update.
    Progress(DownloadProgress),
    /// The active download was canceled by the user.
    Cancelled,
    /// The active update finished (or failed).
    Completed(Result<UpdateOutcome, String>),
    /// The user asked to retry the failed download.
    Retry,
    /// The user dismissed the failed download.
    Dismiss,
}

/// The main application view.
pub struct TesseraApp {
    /// Holds focus whenever nothing else does, so key events keep reaching the window.
    focus_handle: gpui::FocusHandle,
    pub(crate) state: AppState,
    title_bar: Entity<AppTitleBar>,
    display_icon: Option<Arc<RenderImage>>,
    search_input: Entity<InputState>,
    type_filter: Entity<FilterListState>,
    status_filter: Entity<FilterListState>,
    type_filter_options_len: usize,
    status_filter_options_len: usize,
    table: Entity<TableState<AssetTableDelegate>>,
    preview: Entity<PreviewPane>,
    show_preview: bool,
    update_tx: mpsc::Sender<UpdateEvent>,
    // These handles are intentionally retained for their Drop/lifetime
    // semantics; GPUI owns the work they keep alive.
    #[allow(dead_code)]
    pub subscriptions: Vec<Subscription>,
    #[allow(dead_code)]
    pub controller_task: Task<()>,
    #[allow(dead_code)]
    pub update_task: Task<()>,
}

impl TesseraApp {
    /// Creates the main application view.
    pub fn new(state: AppState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let applied_always_on_top = state.settings.always_on_top();
        system::set_always_on_top(window, applied_always_on_top);

        let title_bar = cx.new(|cx| AppTitleBar::new(state.name, state.version, window, cx));

        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search for assets..."));

        let type_filter_options_len = state.asset_type_filter_options.len();
        let type_filter = cx.new(|_| FilterListState::new(state.asset_type_filter_options.clone()));

        let status_filter_options_len = state.asset_status_filter_options.len();
        let status_filter =
            cx.new(|_| FilterListState::new(state.asset_status_filter_options.clone()));

        let delegate = AssetTableDelegate::new(
            state.asset_manager.clone(),
            state.asset_columns.clone(),
            state.asset_type_filter_options.clone(),
            state.asset_status_filter_options.clone(),
            type_filter.clone(),
            status_filter.clone(),
        );

        let table = cx.new(|cx| TableState::new(delegate, window, cx));

        let preview =
            cx.new(|cx| PreviewPane::new(&state.settings, state.asset_manager.clone(), window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&title_bar, window, Self::on_title_bar_event),
            cx.subscribe_in(&table, window, Self::on_table_request),
            cx.subscribe_in(&search_input, window, Self::on_search_event),
            cx.subscribe_in(&type_filter, window, Self::on_filter_event),
            cx.subscribe_in(&status_filter, window, Self::on_filter_event),
        ];

        // Poll the controller channel for messages from background work.
        let poll_controller = state.controller.clone();

        let controller_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;

                let messages = poll_controller.drain();

                if messages.is_empty() {
                    continue;
                }

                let result = this.update_in(cx, |this: &mut Self, window, cx| {
                    for message in messages {
                        this.on_controller_message(message, window, cx);
                    }
                });

                if result.is_err() {
                    break;
                }
            }
        });

        // Poll the update channel for events from update background threads.
        let (update_tx, update_rx) = mpsc::channel::<UpdateEvent>();

        let update_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;

                let mut events = Vec::new();

                while let Ok(event) = update_rx.try_recv() {
                    events.push(event);
                }

                if events.is_empty() {
                    continue;
                }

                let result = this.update_in(cx, |this: &mut Self, window, cx| {
                    for event in events {
                        this.on_update_event(event, window, cx);
                    }
                });

                if result.is_err() {
                    break;
                }
            }
        });

        // Check for updates once per launch, when the app has somewhere to update from.
        if let Some(repository) = state.updates
            && !cfg!(debug_assertions)
        {
            Self::spawn_update_check(repository, state.version, update_tx.clone(), false);
        }

        let display_icon = state
            .asset_manager
            .display_icon()
            .and_then(|icon| icon.to_render_image());

        Self {
            focus_handle: cx.focus_handle(),
            state,
            title_bar,
            display_icon,
            search_input,
            type_filter,
            status_filter,
            type_filter_options_len,
            status_filter_options_len,
            table,
            preview,
            show_preview: cfg!(feature = "start-preview"),
            update_tx,
            subscriptions,
            controller_task,
            update_task,
        }
    }

    /// Whether or not the app is currently loading, exporting, or doing other processing work.
    fn is_busy(&self) -> bool {
        self.state.loading || self.state.exporting || self.state.active_asset_download.is_some()
    }

    /// The current number of selected assets.
    fn selection_len(&self, cx: &Context<Self>) -> usize {
        self.table.read(cx).delegate().selection().len()
    }

    /// Refreshes the asset table.
    ///
    /// Only the rows change here, and the table reads its row count each render. Refreshing it
    /// would rebuild the columns from their configured widths, throwing away whatever the user
    /// resized them to.
    pub(crate) fn refresh_table(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().invalidate();
            cx.notify();
        });

        cx.notify();
    }

    fn on_title_bar_event(
        &mut self,
        _: &Entity<AppTitleBar>,
        event: &TitleBarEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TitleBarEvent::ToggleSettings => self.on_settings(cx),
        }
    }

    /// Occurs when the table requests an action.
    fn on_table_request(
        &mut self,
        _: &Entity<TableState<AssetTableDelegate>>,
        request: &AssetTableRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match request {
            AssetTableRequest::SelectionChanged => cx.notify(),
            AssetTableRequest::Selected => {
                self.request_preview(window, cx);
                cx.notify();
            }
            AssetTableRequest::ExportAsset(index) => {
                if !self.is_busy() {
                    self.on_export(vec![*index], cx);
                }
            }
        }
    }

    /// Occurs when the search input changes or is submitted.
    fn on_search_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                // Clearing the input always resets the results, even when the
                // asset count is too large for realtime searching.
                if self.search_input.read(cx).value().is_empty()
                    || self.state.asset_manager.assets_total() <= SEARCH_REALTIME_MAX
                {
                    self.on_search_submit(cx);
                }
            }
            InputEvent::PressEnter => self.on_search_submit(cx),
            InputEvent::Blur => {}
        }
    }

    fn on_filter_event(
        &mut self,
        _: &Entity<FilterListState>,
        _: &FilterListChanged,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_column_filters(cx);
    }

    fn apply_column_filters(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        let filters = AssetColumnFilters {
            types: selected_filter_values(&self.type_filter, self.type_filter_options_len, cx),
            statuses: selected_filter_values(
                &self.status_filter,
                self.status_filter_options_len,
                cx,
            ),
        };

        self.state.asset_manager.set_column_filters(filters);

        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.delegate_mut().invalidate();
            table.scroll_to_row(0, cx);
            cx.notify();
        });

        cx.notify();
    }

    /// Submits the search term to filter assets.
    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        let search = self.search_input.read(cx).value();

        if search.is_empty() {
            self.state.asset_manager.search(None);
        } else {
            self.state
                .asset_manager
                .search(Some(SearchTerm::compile(&search)));
        }

        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.delegate_mut().invalidate();
            table.scroll_to_row(0, cx);
            cx.notify();
        });

        cx.notify();
    }

    /// Attempts to select files to load.
    fn on_load_file(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        let mut file_dialog = FileDialog::new().set_title(format!(
            "{} | Select game files to load",
            title_case(self.state.name)
        ));

        for (name, extensions) in &self.state.file_filters {
            file_dialog = file_dialog.add_filter(*name, extensions);
        }

        let controller = self.state.controller.clone();

        std::thread::spawn(move || {
            if cfg!(feature = "multi-file") {
                if let Some(files) = file_dialog.pick_files() {
                    controller.load_files(files);
                }
            } else if let Some(file) = file_dialog.pick_file() {
                controller.load_files(vec![file]);
            }
        });

        let _ = cx;
    }

    /// Attempts to select one folder of game files to load.
    ///
    /// The folder is handed to `load_files` as a single path, so an asset manager only ever deals
    /// with one loading entry point however its title stores its assets.
    fn on_load_folder(&self) {
        let file_dialog = FileDialog::new().set_title(format!(
            "{} | Select the game folder to load",
            title_case(self.state.name)
        ));

        let controller = self.state.controller.clone();

        std::thread::spawn(move || {
            if let Some(folder) = file_dialog.pick_folder() {
                controller.load_files(vec![folder]);
            }
        });
    }

    /// Loads the given files on a background thread.
    pub(crate) fn on_load_files(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        self.state.loading = true;
        self.state.last_load = Some(files.clone());

        self.reset_animation_binding(cx);

        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.delegate_mut().set_loading(true);
            cx.notify();
        });

        let manager = self.state.asset_manager.clone();
        let settings = self.state.settings.clone();
        let controller = self.state.controller.clone();

        cx.background_spawn(async move {
            let result = manager.load_files(settings, files.clone());

            controller.load_update(result, Some(files));
        })
        .detach();

        cx.notify();
    }

    /// Loads files received through the window drop queue.
    fn on_load_files_dropped(&mut self, cx: &mut Context<Self>) {
        let mut files = std::mem::take(&mut self.state.files_dropped);

        // A folder loading manager wants the folder itself, and only one of them, so extension
        // matching does not apply.
        if self.state.asset_manager.loads_folders() {
            if let Some(folder) = files.into_iter().find(|path| path.is_dir()) {
                self.on_load_files(vec![folder], cx);
            }

            return;
        }

        // Match porter-app's drop behavior: choose the first configured
        // extension represented by the dropped files, then load only files
        // with that extension.
        let mut target_extension: Option<String> = None;

        'outer: for filter in &self.state.file_filters {
            for file in &files {
                if let Some(extension) = file.extension() {
                    let extension = extension.to_string_lossy().into_owned();

                    if filter.1.iter().any(|&value| value == extension) {
                        target_extension = Some(extension);
                        break 'outer;
                    }
                }
            }
        }

        let Some(target_extension) = target_extension else {
            return;
        };

        let files_to_load: Vec<_> = files
            .drain(..)
            .filter(|file| {
                let Some(extension) = file.extension() else {
                    return false;
                };

                target_extension == extension.to_string_lossy()
            })
            .take(if cfg!(feature = "multi-file") {
                usize::MAX
            } else {
                1
            })
            .collect();

        if !files_to_load.is_empty() {
            self.on_load_files(files_to_load, cx);
        }
    }

    /// Loads a running game instance on a background thread.
    fn on_load_game(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        self.state.loading = true;
        // An empty list marks a game load, so a settings change can reload it; `None` means
        // there is nothing to reload.
        self.state.last_load = Some(Vec::new());

        self.reset_animation_binding(cx);

        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.delegate_mut().set_loading(true);
            cx.notify();
        });

        let manager = self.state.asset_manager.clone();
        let settings = self.state.settings.clone();
        let controller = self.state.controller.clone();

        cx.background_spawn(async move {
            let result = manager.load_game(settings);

            controller.load_update(result, None);
        })
        .detach();

        cx.notify();
    }

    /// Exports the given assets on a background thread.
    fn on_export(&mut self, assets: Vec<usize>, cx: &mut Context<Self>) {
        if self.is_busy() || assets.is_empty() {
            return;
        }

        self.state.exporting = true;
        self.state.export_canceled = false;
        self.state.progress = 0;

        let manager = self.state.asset_manager.clone();
        let settings = self.state.settings.clone();
        let controller = self.state.controller.clone();

        // Export blocks until finished, so it must not run on the ui thread.
        cx.background_spawn(async move {
            manager.export(settings, controller, assets);
        })
        .detach();

        cx.notify();
    }

    /// Exports the selected assets.
    fn on_export_selected(&mut self, cx: &mut Context<Self>) {
        let assets = self
            .table
            .read(cx)
            .delegate()
            .selection()
            .iter()
            .copied()
            .collect();

        self.on_export(assets, cx);
    }

    /// Exports all visible assets.
    fn on_export_all(&mut self, cx: &mut Context<Self>) {
        let assets = (0..self.state.asset_manager.assets_visible()).collect();

        self.on_export(assets, cx);
    }

    /// Cancels an active export.
    fn on_export_cancel(&mut self, cx: &mut Context<Self>) {
        if !self.state.exporting || self.state.export_canceled {
            return;
        }

        self.state.export_canceled = true;
        self.state.asset_manager.export_cancel();

        cx.notify();
    }

    /// Opens the export folder in the platform file explorer.
    fn on_open_export_folder(&mut self) {
        let mut path = self.state.settings.output_directory();

        if let Some(sub_dir) = self
            .state
            .asset_manager
            .game_export_sub_dir(&self.state.settings)
        {
            path = path.join(sub_dir);
        }

        system::open_folder(&path);
    }

    /// Occurs when the user wants to pick a new export folder.
    pub(crate) fn on_pick_export_folder(&mut self) {
        let controller = self.state.controller.clone();

        let file_dialog = FileDialog::new()
            .set_directory(self.state.settings.output_directory())
            .set_title(format!(
                "{} | Select an export directory",
                title_case(self.state.name)
            ));

        std::thread::spawn(move || {
            if let Some(path) = file_dialog.pick_folder() {
                controller.set_output_directory(path);
            }
        });
    }

    /// Handles a message from the controller channel.
    fn on_controller_message(
        &mut self,
        message: ControllerMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            ControllerMessage::LoadFiles(files) => self.on_load_files(files, cx),
            ControllerMessage::LoadFilesDropped => self.on_load_files_dropped(cx),
            ControllerMessage::PreviewUpdate(request_id, asset) => {
                // Model data requested to bind to an animation preview.
                if let Some(binding) = self.state.animation_bind_request
                    && binding.request_id == request_id
                {
                    self.state.animation_bind_request = None;

                    let announce = std::mem::take(&mut self.state.animation_bind_announce);

                    // Some formats resolve their native animation curves against the selected rig.
                    let bound = match &asset {
                        AssetPreview::AnimationWithModel(_, _, model) => Some(model),
                        _ => None,
                    };
                    if let Some(model) = bound {
                        if announce {
                            window.push_notification(
                                Notification::info(format!(
                                    "Animation previews now use {}",
                                    model.name
                                )),
                                cx,
                            );
                        }
                        self.preview
                            .update(cx, |preview, cx| preview.set_preview(asset, cx));
                        return;
                    }

                    let model = match asset {
                        AssetPreview::Model(name, model, images) => Some(crate::AnimationModel {
                            id: binding.model_id,
                            name,
                            model,
                            materials: images.into_iter().map(crate::ModelMaterial::from).collect(),
                        }),
                        AssetPreview::ModelWithMaterials(name, model, materials, _) => {
                            Some(crate::AnimationModel {
                                id: binding.model_id,
                                name,
                                model,
                                materials,
                            })
                        }
                        _ => None,
                    };

                    match &model {
                        Some(model) if announce => window.push_notification(
                            Notification::info(format!(
                                "Animation previews now use {}",
                                model.name
                            )),
                            cx,
                        ),
                        None => window.push_notification(
                            Notification::warning("Failed to load model for animation preview"),
                            cx,
                        ),
                        _ => {}
                    }

                    self.preview.update(cx, |preview, cx| {
                        preview.bind_animation_model(model, cx);
                    });
                } else if request_id == self.state.preview_request_id && self.show_preview {
                    // Ignore stale responses from previous requests.
                    let automatic_model = self.preview.update(cx, |preview, cx| {
                        preview.set_preview(asset, cx);
                        preview.automatic_animation_model()
                    });
                    if let Some(model) = automatic_model {
                        self.on_bind_animation_model(&BindAnimationModelAction(Some(model)), cx);
                    }
                }
            }
            ControllerMessage::LoadUpdate(result, files) => {
                self.state.loading = false;
                match (&result, files) {
                    (Ok(()), Some(files)) => {
                        self.state.last_load = Some(files.clone());
                        self.state
                            .update_settings(|settings| settings.set_last_files(Some(files)));
                    }
                    // A game load keeps the saved last files, as porter-app does.
                    (Ok(()), None) => {}
                    // A failed load must not be retried by the next settings change.
                    (Err(_), _) => self.state.last_load = None,
                }
                if result.is_ok() {
                    self.preview
                        .update(cx, |preview, cx| preview.clear_animation_model(cx));
                }

                self.table.update(cx, |table, cx| {
                    table.delegate_mut().set_loading(false);
                    cx.notify();
                });

                self.refresh_table(cx);

                if result.is_ok() {
                    self.on_search_submit(cx);
                    self.table.update(cx, |table, cx| {
                        table.delegate_mut().reapply_sort();
                        cx.notify();
                    });
                }

                // The display icon refreshes whenever a load finishes.
                self.display_icon = self
                    .state
                    .asset_manager
                    .display_icon()
                    .and_then(|icon| icon.to_render_image());

                match result {
                    Ok(()) => window.push_notification(
                        Notification::success(format!(
                            "Loaded {} assets",
                            format_count(self.state.asset_manager.assets_total())
                        )),
                        cx,
                    ),
                    Err(error) => window.push_notification(Notification::error(error), cx),
                }

                // A settings change made while loading waits for the load to finish.
                self.on_check_reload(cx);
            }
            ControllerMessage::ProgressUpdate(finished, progress) => {
                self.state.progress = progress.min(100);

                if finished {
                    let canceled = self.state.export_canceled;

                    self.state.exporting = false;
                    self.state.export_canceled = false;

                    if canceled {
                        window.push_notification(Notification::info("Export canceled"), cx);
                    } else {
                        window.push_notification(Notification::success("Export finished"), cx);
                    }
                }

                self.refresh_table(cx);

                // A settings change made while exporting waits for the export to finish.
                if finished {
                    self.on_check_reload(cx);
                }
            }
            ControllerMessage::SetOutputDirectory(path) => {
                self.state
                    .update_settings(|settings| settings.set_output_directory(path));
                cx.notify();
            }
            ControllerMessage::Warning(message) => {
                window.push_notification(Notification::warning(message), cx);
            }
            ControllerMessage::DownloadBegin => {
                self.state.active_asset_download = Some(ActiveAssetDownload {
                    name: String::new(),
                    percent: 0,
                    bytes_per_sec: 0,
                    cancel: self.state.controller.download_cancel(),
                });
                cx.notify();
            }
            ControllerMessage::DownloadRequest { url, label, reply } => {
                if let Some(download) = &mut self.state.active_asset_download {
                    download.name = label;
                    download.percent = 0;
                }

                let controller = self.state.controller.clone();
                let progress_controller = controller.clone();
                std::thread::spawn(move || {
                    let value = update::download_bytes(
                        &url,
                        || controller.download_cancel().is_cancelled(),
                        |progress| {
                            progress_controller
                                .download_progress(progress.percent, progress.bytes_per_sec);
                        },
                    )
                    .map_err(|error| match error {
                        UpdateError::Cancelled => DownloadError::Cancelled,
                        error => DownloadError::Failed(error.to_string()),
                    });
                    let _ = reply.send(value);
                });
                cx.notify();
            }
            ControllerMessage::DownloadProgress {
                percent,
                bytes_per_sec,
            } => {
                if let Some(download) = &mut self.state.active_asset_download {
                    download.percent = percent.min(100);
                    download.bytes_per_sec = bytes_per_sec;
                }
                cx.notify();
            }
            ControllerMessage::DownloadEnd => {
                self.state.active_asset_download = None;
                cx.notify();
            }
        }
    }

    /// Toggles the preview pane.
    fn on_preview_toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_preview = !self.show_preview;

        if self.show_preview {
            let settings = self.state.settings.clone();

            self.preview.update(cx, |preview, cx| {
                preview.sync_settings(&settings, cx);
            });

            self.request_preview(window, cx);
        } else {
            // The pane keeps its state while hidden, so a playing sound would carry on with no
            // visible player to stop it.
            self.preview.update(cx, |preview, cx| {
                preview.stop_playback(cx);
            });
        }

        cx.notify();
    }

    /// Occurs when the preview pane requests a model be bound to an animation.
    fn on_bind_animation_model(
        &mut self,
        action: &BindAnimationModelAction,
        cx: &mut Context<Self>,
    ) {
        if !self.preview.read(cx).is_animation_preview() {
            return;
        }

        match action.0 {
            None => {
                self.state.animation_bind_request = None;

                self.preview.update(cx, |preview, cx| {
                    preview.bind_animation_model(None, cx);
                });
            }
            Some(model) => {
                self.state.preview_request_id += 1;

                let request_id = self.state.preview_request_id;

                self.state.animation_bind_request = Some(crate::app_state::AnimationModelRequest {
                    request_id,
                    model_id: model,
                });

                let manager = self.state.asset_manager.clone();
                let settings = self.state.settings.clone();
                let controller = self.state.controller.clone();

                self.preview
                    .update(cx, |preview, cx| preview.set_loading(cx));

                std::thread::spawn(move || {
                    manager.preview_animation_model(settings, model, request_id, controller);
                });
            }
        }
    }

    /// Requests a preview of the first selected asset.
    fn request_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_preview || self.state.asset_manager.assets_empty() {
            return;
        }

        let Some(&index) = self.table.read(cx).delegate().selection().iter().next() else {
            // Selection was cleared: nothing will arrive to replace the current preview, so stop
            // it here instead of leaving it playing.
            self.state.animation_bind_request = None;
            self.preview.update(cx, |preview, cx| {
                preview.stop_playback(cx);
            });
            return;
        };

        // Holding alt forces a raw file preview, like porter-app.
        self.request_preview_of(index, window.modifiers().alt, cx);
    }

    /// Requests a preview of the given asset, raw when the flag is set.
    fn request_preview_of(&mut self, index: usize, raw: bool, cx: &mut Context<Self>) {
        if self.state.asset_manager.assets_empty() {
            return;
        }

        self.state.animation_bind_request = None;
        self.state.preview_request_id += 1;

        let request_id = self.state.preview_request_id;
        let manager = self.state.asset_manager.clone();
        let settings = self.state.settings.clone();
        let controller = self.state.controller.clone();

        self.preview
            .update(cx, |preview, cx| preview.set_loading(cx));

        std::thread::spawn(move || {
            manager.preview(settings, controller, index, raw, request_id);
        });
    }

    /// Occurs when a row context menu asks for a preview: opens the pane when hidden, selects the
    /// row and requests it, as porter-app's `PreviewAsset` message does.
    fn on_preview_asset_action(&mut self, index: usize, raw: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().select_single(index);
            cx.notify();
        });

        if !self.show_preview {
            self.show_preview = true;

            let settings = self.state.settings.clone();

            self.preview.update(cx, |preview, cx| {
                preview.sync_settings(&settings, cx);
            });
        }

        self.request_preview_of(index, raw, cx);
        cx.notify();
    }

    /// Moves a single row selection up or down, previewing and scrolling to the new row.
    ///
    /// As in porter-app, nothing happens with no selection or more than one row selected, and
    /// the selection stops at the ends rather than wrapping.
    fn move_selection(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.state.asset_manager.assets_visible();
        let selection = self.table.read(cx).delegate().selection();

        if selection.len() != 1 {
            return;
        }

        let Some(&index) = selection.first() else {
            return;
        };

        let target = if down {
            if index + 1 >= rows {
                return;
            }

            index + 1
        } else {
            if index == 0 {
                return;
            }

            index - 1
        };

        self.table.update(cx, |table, cx| {
            table.delegate_mut().select_single(target);
            table.scroll_row_into_view(target, cx);
        });

        self.request_preview(window, cx);
        cx.notify();
    }

    /// Toggles the settings view.
    pub(crate) fn on_settings(&mut self, cx: &mut Context<Self>) {
        self.state.show_settings = !self.state.show_settings;

        if !self.state.show_settings {
            // Apply preview related settings changes (volume, controls, far clip).
            let settings = self.state.settings.clone();

            self.preview.update(cx, |preview, cx| {
                preview.sync_settings(&settings, cx);
            });

            self.on_check_reload(cx);
        }

        cx.notify();
    }

    fn sync_always_on_top(&mut self, window: &mut Window) {
        let enabled = self.state.settings.always_on_top();
        if self.state.applied_always_on_top == enabled {
            return;
        }

        system::set_always_on_top(window, enabled);
        self.state.applied_always_on_top = enabled;
    }

    /// Checks the configured app release channel on a background thread.
    fn spawn_update_check(
        repository: UpdateRepository,
        version: &'static str,
        tx: mpsc::Sender<UpdateEvent>,
        manual: bool,
    ) {
        std::thread::spawn(move || {
            let proxy = update::read_github_proxy_config();
            let mut found = false;

            if let Some(info) = update::check_app_update(&proxy, repository, version) {
                found = true;
                let _ = tx.send(UpdateEvent::Available(info));
            }

            if manual && !found {
                let _ = tx.send(UpdateEvent::UpToDate);
            }
        });
    }

    /// Manually checks for updates, from the about settings page.
    pub(crate) fn on_check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.state.updates else {
            return;
        };

        if self.state.checking_updates
            || self.state.pending_update.is_some()
            || self.state.active_download.is_some()
        {
            return;
        }

        self.state.checking_updates = true;

        Self::spawn_update_check(repository, self.state.version, self.update_tx.clone(), true);

        cx.notify();
    }

    /// Whether a manual update check is currently running.
    pub(crate) fn is_checking_updates(&self) -> bool {
        self.state.checking_updates
    }

    /// Handles an event from the update channel.
    fn on_update_event(&mut self, event: UpdateEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            UpdateEvent::Available(info) => {
                self.state.checking_updates = false;
                self.state.update_queue.push_back(info);
                self.prompt_next_update(window, cx);
            }
            UpdateEvent::UpToDate => {
                self.state.checking_updates = false;
                window.push_notification(Notification::success("You are up to date!"), cx);
            }
            UpdateEvent::Accepted => {
                let Some(info) = self.state.pending_update.take() else {
                    return;
                };

                let label = match info.kind {
                    UpdateKind::App => {
                        format!("{} {}", title_case(self.state.name), info.remote_version)
                    }
                };

                self.state.active_download = Some(ActiveDownload {
                    info,
                    label,
                    percent: 0,
                    bytes_per_sec: 0,
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    failed: None,
                });

                self.start_download();
            }
            UpdateEvent::Declined => {
                if self.state.pending_update.take().is_some() {
                    self.prompt_next_update(window, cx);
                }
            }
            UpdateEvent::Progress(progress) => {
                if let Some(download) = &mut self.state.active_download {
                    download.percent = progress.percent.min(100);
                    download.bytes_per_sec = progress.bytes_per_sec;
                }
            }
            UpdateEvent::Cancelled => {
                self.state.active_download = None;
                self.prompt_next_update(window, cx);
            }
            UpdateEvent::Completed(result) => {
                match result {
                    // App updates restart the process and won't reach here.
                    Ok(UpdateOutcome::AppInstalled) => {
                        self.state.active_download = None;
                    }
                    Err(error) => {
                        // Keep the download around so it can be retried.
                        if let Some(download) = &mut self.state.active_download {
                            download.failed = Some(error);
                        }
                    }
                }

                self.prompt_next_update(window, cx);
            }
            UpdateEvent::Retry => {
                self.start_download();
            }
            UpdateEvent::Dismiss => {
                self.state.active_download = None;
                self.prompt_next_update(window, cx);
            }
        }

        cx.notify();
    }

    /// Starts (or restarts) the active update download on a background thread.
    fn start_download(&mut self) {
        let Some(download) = &mut self.state.active_download else {
            return;
        };

        download.percent = 0;
        download.bytes_per_sec = 0;
        download.failed = None;
        download.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let info = download.info.clone();
        let cancel = download.cancel.clone();
        let tx = self.update_tx.clone();
        let progress_tx = tx.clone();

        std::thread::spawn(move || {
            let result = update::perform_update(info, cancel, move |progress| {
                let _ = progress_tx.send(UpdateEvent::Progress(progress));
            });

            let event = match result {
                Ok(outcome) => UpdateEvent::Completed(Ok(outcome)),
                Err(UpdateError::Cancelled) => UpdateEvent::Cancelled,
                Err(error) => UpdateEvent::Completed(Err(error.to_string())),
            };

            let _ = tx.send(event);
        });
    }

    /// Prompts for the next queued update, if nothing else is in flight.
    fn prompt_next_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.pending_update.is_some() || self.state.active_download.is_some() {
            return;
        }

        let Some(info) = self.state.update_queue.pop_front() else {
            return;
        };

        let (title, description) = match info.kind {
            UpdateKind::App => (
                "Update Available",
                format!(
                    "{} {} is available (you have v{}). The app will restart after updating.",
                    title_case(self.state.name),
                    info.remote_version,
                    info.local_version
                ),
            ),
        };

        self.state.pending_update = Some(info);

        let prompt = cx.new(|_| UpdatePrompt {
            title,
            description,
            tx: self.update_tx.clone(),
        });

        window.open_dialog(prompt, cx);

        cx.notify();
    }

    /// Renders the update download overlay.
    fn render_update_overlay(&self, cx: &gpui::App) -> impl IntoElement + use<> {
        let theme = *cx.tessera_theme();

        let (label, percent, bytes_per_sec, failed) = match &self.state.active_download {
            Some(download) => (
                download.label.clone(),
                download.percent,
                download.bytes_per_sec,
                download.failed.clone(),
            ),
            None => (String::new(), 0, 0, None),
        };

        let mut panel = v_flex()
            .w(px(420.0))
            .p_6()
            .gap_3()
            .rounded_lg()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if failed.is_some() {
                        "Download failed"
                    } else {
                        "Downloading update"
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.text_secondary)
                    .child(label),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().flex_1().child(Progress::new(percent as f32)))
                    .child(
                        div()
                            .w(px(38.0))
                            .text_size(px(12.5))
                            .text_color(theme.text_secondary)
                            .child(format!("{percent}%")),
                    ),
            );

        if let Some(error) = failed {
            let retry_tx = self.update_tx.clone();
            let dismiss_tx = self.update_tx.clone();

            panel = panel
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.warning)
                        .child(error),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .pt_1()
                        .child(
                            Button::new("update-dismiss")
                                .flex_1()
                                .outline()
                                .label("Dismiss")
                                .on_click(move |_, _, _| {
                                    let _ = dismiss_tx.send(UpdateEvent::Dismiss);
                                }),
                        )
                        .child(
                            Button::new("update-retry")
                                .flex_1()
                                .primary()
                                .label("Retry")
                                .on_click(move |_, _, _| {
                                    let _ = retry_tx.send(UpdateEvent::Retry);
                                }),
                        ),
                );
        } else {
            let cancel = self
                .state
                .active_download
                .as_ref()
                .map(|download| download.cancel.clone());

            panel = panel.child(
                h_flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(format_bytes_per_sec(bytes_per_sec)),
                    )
                    .child(
                        Button::new("update-cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(move |_, _, _| {
                                if let Some(cancel) = &cancel {
                                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }),
                    ),
            );
        }

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.scrim)
            .child(panel)
    }

    /// Renders the single modal used by core asset downloads.
    fn render_asset_download_overlay(&self, cx: &gpui::App) -> impl IntoElement + use<> {
        let theme = *cx.tessera_theme();
        let (name, percent, bytes_per_sec, cancel) = match &self.state.active_asset_download {
            Some(download) => (
                download.name.clone(),
                download.percent,
                download.bytes_per_sec,
                Some(download.cancel.clone()),
            ),
            None => (String::new(), 0, 0, None),
        };

        let panel = v_flex()
            .w(px(420.0))
            .p_6()
            .gap_3()
            .rounded_lg()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Downloading {name}")),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().flex_1().child(Progress::new(percent as f32)))
                    .child(
                        div()
                            .w(px(38.0))
                            .text_size(px(12.5))
                            .text_color(theme.text_secondary)
                            .child(format!("{percent}%")),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(format_bytes_per_sec(bytes_per_sec)),
                    )
                    .child(
                        Button::new("asset-download-cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(move |_, _, _| {
                                if let Some(cancel) = &cancel {
                                    cancel.signal();
                                }
                            }),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.scrim)
            .child(panel)
    }

    /// Reloads the last load, if required by a settings change.
    fn on_check_reload(&mut self, cx: &mut Context<Self>) {
        if !self.state.reload_required || self.is_busy() || self.state.show_settings {
            cx.notify();
            return;
        }

        self.state.reload_required = false;

        match self.state.last_load.clone() {
            Some(files) if !files.is_empty() => self.on_load_files(files, cx),
            Some(_) => self.on_load_game(cx),
            None => {}
        }

        cx.notify();
    }

    /// Copies the selected asset names to the clipboard.
    fn on_copy_text(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        let buffer = self
            .table
            .read(cx)
            .delegate()
            .selection()
            .iter()
            .copied()
            .map(|index| self.state.asset_manager.assets_info(index))
            .filter_map(|mut info| {
                if info.is_empty() {
                    None
                } else {
                    Some(info.remove(0).0)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if !buffer.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(buffer));
        }
    }

    /// Occurs when a key is pressed anywhere in the window.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        // Plain-key shortcuts must not fire while typing in the search box or
        // the preview's animation model search.
        let typing = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
            || self.preview.read(cx).is_text_input_focused(window, cx);

        match keystroke.key.as_str() {
            "c" if keystroke.modifiers.secondary() => self.on_copy_text(cx),
            "f" if keystroke.modifiers.secondary() => {
                self.search_input.update(cx, |input, cx| {
                    input.focus(window, cx);
                });
            }
            _ if typing => {}
            // The list keys work whenever the window has focus, not only with the table focused.
            "up" if !keystroke.modifiers.modified() && !self.state.show_settings => {
                self.move_selection(false, window, cx);
            }
            "down" if !keystroke.modifiers.modified() && !self.state.show_settings => {
                self.move_selection(true, window, cx);
            }
            "pageup" if !keystroke.modifiers.modified() && !self.state.show_settings => {
                self.table
                    .update(cx, |table, cx| table.scroll_page(false, cx));
            }
            "pagedown" if !keystroke.modifiers.modified() && !self.state.show_settings => {
                self.table
                    .update(cx, |table, cx| table.scroll_page(true, cx));
            }
            "e" if !keystroke.modifiers.modified() && self.selection_len(cx) > 0 => {
                self.on_export_selected(cx);
            }
            "p" if !keystroke.modifiers.modified() && !self.state.show_settings => {
                self.on_preview_toggle(window, cx);
            }
            key if !keystroke.modifiers.modified()
                && self.show_preview
                && !self.state.show_settings =>
            {
                let key = key.to_string();

                self.preview.update(cx, |preview, cx| {
                    preview.handle_key(&key, cx);
                });
            }
            _ => {}
        }
    }

    /// Occurs when a row context menu dispatches one of its actions.
    fn on_asset_action(&mut self, action: &AssetAction, cx: &mut Context<Self>) {
        match *action {
            AssetAction::Export(index) => self.on_export_asset_action(index, cx),
            AssetAction::Preview(index, raw) => self.on_preview_asset_action(index, raw, cx),
            AssetAction::CopyColumn(index, column) => self.on_copy_column_action(index, column, cx),
            AssetAction::UseAsAnimationPreview(index) => self.on_use_as_animation_preview(index),
        }
    }

    /// Occurs when a row context menu requests an export.
    fn on_export_asset_action(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }

        // Export the whole selection when the asset is part of it.
        let selection = self.table.read(cx).delegate().selection();

        let assets = if selection.contains(&index) {
            selection.iter().copied().collect()
        } else {
            vec![index]
        };

        self.on_export(assets, cx);
    }

    /// Drops the remembered animation preview model before a load replaces the asset list.
    ///
    /// Called only from the load paths, not when the table is filtered or searched: narrowing the
    /// view does not invalidate a binding, but replacing what it points at does.
    fn reset_animation_binding(&mut self, cx: &mut Context<Self>) {
        self.state.animation_bind_request = None;
        self.state.animation_bind_announce = false;

        self.preview.update(cx, |preview, cx| {
            preview.clear_animation_model(cx);
        });
    }

    /// Occurs when a row context menu binds a model to animation previews.
    ///
    /// The model is remembered by the preview pane, so every animation previewed afterwards binds
    /// to it until a new load replaces the asset list.
    fn on_use_as_animation_preview(&mut self, index: usize) {
        if self.is_busy() {
            return;
        }

        let Some(model) = self.state.asset_manager.animation_preview_model_at(index) else {
            return;
        };

        self.state.preview_request_id += 1;

        let request_id = self.state.preview_request_id;

        self.state.animation_bind_request = Some(crate::app_state::AnimationModelRequest {
            request_id,
            model_id: model,
        });
        self.state.animation_bind_announce = true;

        let manager = self.state.asset_manager.clone();
        let settings = self.state.settings.clone();
        let controller = self.state.controller.clone();

        // Deliberately no `set_loading` here, unlike the preview header's picker: this runs while
        // the user is looking at something else, and blanking that preview to decode a model they
        // did not ask to view would be wrong. The bind lands when the response arrives.
        std::thread::spawn(move || {
            manager.preview_animation_model(settings, model, request_id, controller);
        });
    }

    /// Occurs when a row context menu requests a column copy.
    fn on_copy_column_action(&mut self, index: usize, col_ix: usize, cx: &mut Context<Self>) {
        if self.state.loading {
            return;
        }

        let Some((value, _)) = self
            .state
            .asset_manager
            .assets_info(index)
            .into_iter()
            .nth(col_ix)
        else {
            return;
        };

        cx.write_to_clipboard(ClipboardItem::new_string(value));
    }

    /// Renders the application logo mark, or the manager's display icon.
    pub(crate) fn render_logo(&self, size: f32, cx: &gpui::App) -> impl IntoElement + use<> {
        let theme = *cx.tessera_theme();

        let base = div()
            .w(px(size))
            .h(px(size))
            .rounded(px(size * 0.28))
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .border_1()
            .border_color(palette::with_alpha(theme.primary_hover, 0.4));

        // if let Some(icon) = &self.display_icon {
        //     return base.child(img(icon.clone()).w_full().h_full());
        // }

        let initial = self
            .state
            .name
            .chars()
            .next()
            .map(|first| first.to_uppercase().to_string())
            .unwrap_or_default();

        base.bg(theme.primary_active)
            .text_color(theme.on_primary)
            .text_size(px(size * 0.55))
            .font_weight(FontWeight::EXTRA_BOLD)
            .child(initial)
    }

    /// Renders the search bar component.
    fn render_search_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let busy = self.is_busy();
        let search_empty = self.search_input.read(cx).value().is_empty();
        let selected = self.selection_len(cx);

        let status: SharedString = if self.state.loading {
            "Loading assets...".into()
        } else if search_empty {
            format!(
                "{} assets loaded",
                format_count(self.state.asset_manager.assets_visible())
            )
            .into()
        } else {
            format!(
                "Showing {} of {} assets",
                format_count(self.state.asset_manager.assets_visible()),
                format_count(self.state.asset_manager.assets_total())
            )
            .into()
        };

        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .flex_shrink_0()
            .child(
                div().w(px(360.0)).child(
                    Input::new(&self.search_input)
                        .prefix(
                            Icon::new(IconName::Search)
                                .size_4()
                                .text_color(cx.tessera_theme().text_muted),
                        )
                        .cleanable(true)
                        .disabled(busy),
                ),
            )
            .when(
                self.state.asset_manager.assets_total() > SEARCH_REALTIME_MAX,
                |this| {
                    this.child(
                        Button::new("search")
                            .label("Search")
                            .outline()
                            .disabled(search_empty || busy)
                            .on_click(cx.listener(|this, _, _, cx| this.on_search_submit(cx))),
                    )
                },
            )
            .child(
                h_flex()
                    .flex_1()
                    .justify_end()
                    .gap_3()
                    .items_center()
                    .when(selected > 0, |this| {
                        this.child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded_full()
                                .text_size(px(12.0))
                                .bg(palette::with_alpha(cx.tessera_theme().primary, 0.15))
                                .text_color(cx.tessera_theme().primary_hover)
                                .child(format!("{} selected", format_count(selected))),
                        )
                    })
                    .when(self.state.loading, |this| {
                        this.child(Spinner::new().small())
                    })
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(cx.tessera_theme().text_secondary)
                            .child(status),
                    ),
            )
    }

    /// Renders the controls component.
    fn render_controls(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let busy = self.is_busy();
        let selected = self.selection_len(cx);
        let assets_empty = self.state.asset_manager.assets_empty();

        let export_selected_label: SharedString = if selected > 0 {
            format!("Export Selected ({})", format_count(selected)).into()
        } else {
            "Export Selected".into()
        };

        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .flex_shrink_0()
            .bg(cx.tessera_theme().surface)
            .border_t_1()
            .border_color(cx.tessera_theme().border)
            .when(self.state.asset_manager.supports_games(), |this| {
                this.child(
                    Button::new("load-game")
                        .icon(IconName::Gamepad)
                        .label("Load Game")
                        .outline()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.on_load_game(cx))),
                )
            })
            .when(self.state.asset_manager.supports_files(), |this| {
                // Load files
                this.child(
                    Button::new("load-file")
                        .icon(IconName::FolderOpen)
                        .label("Load Files")
                        .outline()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.on_load_file(cx))),
                )
                // Load folder
                .child(
                    Button::new("load-folder")
                        .icon(IconName::FolderOpen)
                        .label("Load Folder")
                        .outline()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, _| this.on_load_folder())),
                )
            })
            .child(
                // The rule takes no room of its own: it overhangs a zero-width slot.
                div().h(px(20.0)).w(px(0.0)).child(
                    div()
                        .h_full()
                        .w(px(1.0))
                        .bg(cx.tessera_theme().border_strong),
                ),
            )
            .child(
                Button::new("export-selected")
                    .icon(IconName::Download)
                    .label(export_selected_label)
                    .primary()
                    .disabled(selected == 0 || busy)
                    .on_click(cx.listener(|this, _, _, cx| this.on_export_selected(cx))),
            )
            .child(
                Button::new("export-all")
                    .label("Export All")
                    .outline()
                    .disabled(assets_empty || busy)
                    .on_click(cx.listener(|this, _, _, cx| this.on_export_all(cx))),
            )
            .child(
                Button::new("open-export-folder")
                    .icon(IconName::Folder)
                    .ghost()
                    .tooltip("Open Export Folder")
                    .on_click(cx.listener(|this, _, _, _| this.on_open_export_folder())),
            )
            .child(div().flex_1())
            .when(self.state.exporting, |this| {
                this.child(
                    h_flex()
                        .flex_1()
                        .justify_end()
                        .items_center()
                        .gap_3()
                        .child(
                            Button::new("export-cancel")
                                .icon(IconName::CircleX)
                                .label(if self.state.export_canceled {
                                    "Canceling..."
                                } else {
                                    "Cancel"
                                })
                                .ghost()
                                .disabled(self.state.export_canceled)
                                .on_click(cx.listener(|this, _, _, cx| this.on_export_cancel(cx))),
                        )
                        .child(
                            div()
                                .w(px(180.0))
                                .child(Progress::new(self.state.progress as f32)),
                        )
                        .child(
                            div()
                                .w(px(38.0))
                                .text_size(px(12.5))
                                .text_color(cx.tessera_theme().text_secondary)
                                .child(format!("{}%", self.state.progress)),
                        ),
                )
            })
    }

    /// Renders the asset list content, with the preview pane when open.
    fn render_content(&mut self) -> impl IntoElement + use<> {
        let table = DataTable::new(&self.table);

        let content = v_flex()
            .flex_1()
            .w_full()
            .min_h_0()
            .px_3()
            .overflow_hidden();

        if !self.show_preview {
            return content.child(table);
        }

        content.child(
            h_flex()
                .id("content-split")
                .flex_1()
                .w_full()
                .min_h_0()
                .map(|mut this| {
                    this.style().align_items = Some(gpui::AlignItems::Stretch);
                    this
                })
                .child(div().flex_1().min_w_0().min_h_0().pr_2().child(table))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(self.preview.clone()),
                ),
        )
    }
}

impl Render for TesseraApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_always_on_top(window);

        let settings_open = self.state.show_settings;
        self.title_bar.update(cx, |title_bar, _| {
            title_bar.sync_settings(settings_open);
        });

        let busy = self.is_busy();
        self.table
            .update(cx, |table, _| table.delegate_mut().set_busy(busy));

        // gpui drops key events when the focused element is gone (a closed popover, a hidden
        // tab), so take the focus back whenever nothing holds it.
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle);
        }

        // Focus lives on the body, not the window root: gpui focuses a tracked element on every
        // mouse down and marks the event handled, which on Windows would swallow the title bar's
        // non-client clicks (drag, minimize, maximize, close) before the OS sees them.
        let mut body = v_flex()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event, window, cx| this.on_key_down(event, window, cx)),
            );

        if self.state.show_settings {
            body = body.child(self.render_settings(window, cx));
        } else {
            body = body
                .child(self.render_search_bar(cx))
                .child(self.render_content())
                .child(self.render_controls(cx));
        }

        let mut base = v_flex()
            .size_full()
            .relative()
            .bg(cx.tessera_theme().background)
            .text_color(cx.tessera_theme().text)
            .on_action(
                cx.listener(|this, action: &AssetAction, _, cx| this.on_asset_action(action, cx)),
            )
            .on_action(
                cx.listener(|this, action: &BindAnimationModelAction, _, cx| {
                    this.on_bind_animation_model(action, cx)
                }),
            )
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, _cx| {
                let files = paths.paths().to_vec();

                if !files.is_empty() && this.state.asset_manager.supports_files() {
                    this.state.files_dropped.extend(files);
                    this.state.controller.load_files_dropped();
                }
            }))
            .child(self.title_bar.clone())
            .child(body);

        if self.state.active_asset_download.is_some() {
            base = base.child(self.render_asset_download_overlay(cx));
        } else if self.state.active_download.is_some() {
            base = base.child(self.render_update_overlay(cx));
        }

        base
    }
}

/// The modal prompt offering a found update.
struct UpdatePrompt {
    title: &'static str,
    description: String,
    tx: mpsc::Sender<UpdateEvent>,
}

impl Render for UpdatePrompt {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let later_tx = self.tx.clone();
        let update_tx = self.tx.clone();

        Dialog::new(self.title, self.description.clone())
            .footer(
                Button::new("update-later")
                    .flex_1()
                    .outline()
                    .label("Later")
                    .on_click(move |_, window, cx| {
                        let _ = later_tx.send(UpdateEvent::Declined);
                        window.close_dialog(cx);
                    }),
            )
            .footer(
                Button::new("update-now")
                    .flex_1()
                    .primary()
                    .label("Update Now")
                    .on_click(move |_, window, cx| {
                        let _ = update_tx.send(UpdateEvent::Accepted);
                        window.close_dialog(cx);
                    }),
            )
    }
}

/// Formats a download speed in bytes per second as a human readable string.
fn format_bytes_per_sec(bytes_per_sec: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let bytes = bytes_per_sec as f64;

    if bytes >= GB {
        format!("{:.1} GB/s", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB/s", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} KB/s", bytes / KB)
    } else {
        format!("{bytes_per_sec} B/s")
    }
}

/// Converts the first character to uppercase.
pub(crate) fn title_case(value: &str) -> String {
    let mut chars = value.chars();

    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Formats a count with thousands separators.
pub(crate) fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }

        result.push(digit);
    }

    result
}

fn selected_filter_values(
    filter: &Entity<FilterListState>,
    options_len: usize,
    cx: &Context<TesseraApp>,
) -> Option<BTreeSet<String>> {
    let filter = filter.read(cx);

    if filter.selection().len() == options_len {
        return None;
    }

    Some(
        filter
            .selected_values()
            .into_iter()
            .map(str::to_string)
            .collect(),
    )
}
