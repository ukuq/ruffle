use crate::custom_event::{OpenType, RuffleEvent};
use crate::gui::dialogs::Dialogs;
use crate::gui::{DebugMessage, text};
use crate::player::LaunchOptions;
use crate::preferences::GlobalPreferences;
use egui::{Button, Key, KeyboardShortcut, Modifiers, Widget};
use rfd::{MessageButtons, MessageDialog, MessageLevel};
use ruffle_core::config::Letterbox;
use ruffle_core::focus_tracker::DisplayObject;
use ruffle_core::{Player, StageScaleMode};
use ruffle_frontend_utils::backends::navigator::{
    clear_seer2_file_cache, reset_seer2_version_manifest, seer2_cache_metrics,
};
use ruffle_frontend_utils::content::ContentDescriptor;
use ruffle_frontend_utils::recents::Recent;
use ruffle_render::quality::StageQuality;
use unic_langid::LanguageIdentifier;
use url::Url;
use winit::event_loop::EventLoopProxy;

const SEER2_GAME_HOME_URL: &str = "http://seer2.client/seer2/Client.swf";

pub struct MenuBar {
    event_loop: EventLoopProxy<RuffleEvent>,
    pub(super) default_launch_options: LaunchOptions,
    preferences: GlobalPreferences,

    cached_recents: Option<Vec<Recent>>,
    pub currently_opened: Option<(ContentDescriptor, LaunchOptions)>,
    confirm_clear_file_cache: bool,
    focus_clear_file_cache_no: bool,
}

impl MenuBar {
    const SHORTCUT_FULLSCREEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F11);
    const SHORTCUT_FULLSCREEN_WINDOWS: KeyboardShortcut =
        KeyboardShortcut::new(Modifiers::ALT, Key::Enter);
    const SHORTCUT_OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
    const SHORTCUT_OPEN_ADVANCED: KeyboardShortcut =
        KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::O);
    const SHORTCUT_PAUSE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::P);
    const SHORTCUT_STEP: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Space);
    const SHORTCUT_QUIT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Q);

    pub fn new(
        event_loop: EventLoopProxy<RuffleEvent>,
        default_launch_options: LaunchOptions,
        preferences: GlobalPreferences,
    ) -> Self {
        Self {
            event_loop,
            default_launch_options,
            cached_recents: None,
            currently_opened: None,
            confirm_clear_file_cache: false,
            focus_clear_file_cache_no: false,
            preferences,
        }
    }

    pub fn consume_shortcuts(
        &self,
        egui_ctx: &egui::Context,
        dialogs: &mut Dialogs,
        mut player: Option<&mut Player>,
    ) {
        // TODO(mike): Make some MenuItem struct with shortcut info to handle this more cleanly.
        if egui_ctx.input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_OPEN_ADVANCED)) {
            dialogs.open_file_advanced();
        }
        if egui_ctx.input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_OPEN)) {
            self.browse_and_open(OpenType::File);
        }
        if egui_ctx.input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_QUIT)) {
            self.request_exit();
        }

        if let Some(player) = &mut player {
            let playing = player.is_playing();
            if egui_ctx.input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_PAUSE)) {
                player.set_is_playing(!playing);
            }
            if !playing && egui_ctx.input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_STEP))
            {
                player.suspend_after_next_frame();
            }
        }

        let mut fullscreen_pressed =
            egui_ctx.input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_FULLSCREEN));
        if cfg!(windows) && !fullscreen_pressed {
            // TODO We can remove this shortcut when we add some kind of preferences.
            fullscreen_pressed = egui_ctx
                .input_mut(|input| input.consume_shortcut(&Self::SHORTCUT_FULLSCREEN_WINDOWS));
        }
        if let Some(player) = &mut player
            && fullscreen_pressed
        {
            let is_fullscreen = player.is_fullscreen();
            player.set_fullscreen(!is_fullscreen);
        }
    }

    pub fn show(
        &mut self,
        locale: &LanguageIdentifier,
        egui_ui: &mut egui::Ui,
        dialogs: &mut Dialogs,
        mut player: Option<&mut Player>,
    ) {
        egui::Panel::top("menu_bar").show(egui_ui, |ui| {
             egui::MenuBar::new().ui(ui, |ui| {
                self.file_menu(locale, ui, dialogs, player.is_some());
                self.view_menu(locale, ui, &mut player);
                self.controls_menu(locale, ui, dialogs, &mut player);
                ui.menu_button( text(locale, "bookmarks-menu"), |ui| {
                    if Button::new(text(locale, "bookmarks-menu-add")).ui(ui).clicked() {
                        ui.close();

                        let content_descriptor = self.currently_opened.as_ref().map(|(desc, _)| desc.clone());
                        dialogs.open_add_bookmark(content_descriptor);
                    }

                    if Button::new(text(locale, "bookmarks-menu-manage")).ui(ui).clicked() {
                        ui.close();
                        dialogs.open_bookmarks();
                    }

                    if self.preferences.have_bookmarks() {
                        ui.separator();
                        self.preferences.bookmarks(|bookmarks| {
                            for bookmark in bookmarks.iter().filter(|x| !x.is_invalid()) {
                                if Button::new(&bookmark.name).ui(ui).clicked() {
                                    ui.close();
                                    let _ = self.event_loop.send_event(RuffleEvent::Open(
                                        bookmark.content_descriptor.clone(),
                                        Box::new(self.default_launch_options.clone()),
                                    ));
                                }
                            }
                        });
                    }
                });
                ui.menu_button(text(locale, "debug-menu"), |ui| {
                    ui.add_enabled_ui(player.is_some(), |ui| {
                        if Button::new(text(locale, "debug-menu-open-stage")).ui(ui).clicked() {
                            ui.close();
                            if let Some(player) = &mut player {
                                player.debug_ui().queue_message(DebugMessage::TrackStage);
                            }
                        }
                        if let Some(player) = &mut player {
                            let mut has_root_movie_clip = false;
                            player.mutate_with_update_context(|ctx| {
                                has_root_movie_clip = matches!(ctx.stage.root_clip(), Some(DisplayObject::MovieClip(_)));
                            });
                            let button = Button::new(text(locale, "debug-menu-open-root-movie-clip"));
                            if ui.add_enabled(has_root_movie_clip, button).clicked() {
                                ui.close();
                                player.debug_ui().queue_message(DebugMessage::TrackRootMovieClip);
                            }
                        }
                        ui.separator();
                        if Button::new(text(locale, "debug-menu-open-movie")).ui(ui).clicked() {
                            ui.close();
                            if let Some(player) = &mut player {
                                player.debug_ui().queue_message(DebugMessage::TrackTopLevelMovie);
                            }
                        }
                        if Button::new(text(locale, "debug-menu-open-movie-list")).ui(ui).clicked() {
                            ui.close();
                            if let Some(player) = &mut player {
                                player.debug_ui().queue_message(DebugMessage::ShowKnownMovies);
                            }
                        }
                        if Button::new(text(locale, "debug-menu-open-domain-list")).ui(ui).clicked() {
                            ui.close();
                            if let Some(player) = &mut player {
                                player.debug_ui().queue_message(DebugMessage::ShowDomains);
                            }
                        }
                        ui.separator();
                        if Button::new(text(locale, "debug-menu-search-display-objects")).ui(ui).clicked() {
                            ui.close();
                            if let Some(player) = &mut player {
                                player.debug_ui().queue_message(DebugMessage::SearchForDisplayObject);
                            }
                        }
                        ui.separator();
                        if Button::new(text(locale, "debug-menu-network-monitor"))
                            .ui(ui)
                            .clicked()
                        {
                            ui.close();
                            let _ = self
                                .event_loop
                                .send_event(RuffleEvent::OpenNetworkMonitor);
                        }
                    });
                });
                ui.menu_button(text(locale, "help-menu"), |ui| {
                    if ui.button(text(locale, "help-menu-join-discord")).clicked() {
                        self.launch_website(ui, "https://discord.gg/ruffle");
                    }
                    if ui.button(text(locale, "help-menu-report-a-bug")).clicked() {
                        self.launch_website(ui, "https://github.com/ruffle-rs/ruffle/issues/new?assignees=&labels=bug&projects=&template=bug_report.yml");
                    }
                    if ui.button(text(locale, "help-menu-sponsor-development")).clicked() {
                        self.launch_website(ui, "https://opencollective.com/ruffle/");
                    }
                    if ui.button(text(locale, "help-menu-translate-ruffle")).clicked() {
                        self.launch_website(ui, "https://crowdin.com/project/ruffle");
                    }
                    ui.separator();
                    if ui.button(text(locale, "help-menu-about")).clicked() {
                        dialogs.open_about_screen();
                        ui.close();
                    }
                });
                self.cache_metrics_menu(locale, ui);
                self.seer2_proxy_button(locale, ui);
            });
        });
        self.show_clear_file_cache_confirmation(locale, egui_ui.ctx());
    }

    fn cache_metrics_menu(&mut self, locale: &LanguageIdentifier, ui: &mut egui::Ui) {
        let metrics = seer2_cache_metrics();
        let label = format!(
            "{} hit:{} expired:{} fetch:{} cached:{} checked:{} unchanged:{} changed:{} proxy:{}",
            text(locale, "cache-metrics-menu"),
            metrics.hit,
            metrics.expired,
            metrics.fetch,
            metrics.cached,
            metrics.checked,
            metrics.unchanged,
            metrics.changed,
            metrics.proxy,
        );
        ui.menu_button(label, |ui| {
            if ui
                .button(text(locale, "cache-metrics-refresh-manifest"))
                .on_hover_text(text(locale, "cache-metrics-refresh-manifest-tooltip"))
                .clicked()
            {
                reset_seer2_version_manifest();
                tracing::info!("Reset the cached Seer2 version root and Bloom manifest");
                ui.close();
            }
            if ui
                .button(text(locale, "cache-metrics-clear-files"))
                .on_hover_text(text(locale, "cache-metrics-clear-files-tooltip"))
                .clicked()
            {
                ui.close();
                self.confirm_clear_file_cache = true;
                self.focus_clear_file_cache_no = true;
            }
        });
    }

    fn seer2_proxy_button(&self, locale: &LanguageIdentifier, ui: &mut egui::Ui) {
        let options = self
            .currently_opened
            .as_ref()
            .map(|(_, options)| options)
            .unwrap_or(&self.default_launch_options);
        let proxy_root = options.seer2_proxy_root.as_ref();
        let label = if let Some(proxy_root) = proxy_root {
            format!(
                "✅{}({})",
                text(locale, "seer2-proxy-label"),
                proxy_root.display()
            )
        } else {
            format!("❌{}", text(locale, "seer2-proxy-label"))
        };
        let tooltip = if proxy_root.is_some() {
            text(locale, "seer2-proxy-disable-tooltip")
        } else {
            text(locale, "seer2-proxy-enable-tooltip")
        };
        let can_change = self.currently_opened.is_some() && options.seer2_virtual_http;

        if ui
            .add_enabled(can_change, Button::new(label))
            .on_hover_text(tooltip)
            .clicked()
        {
            ui.close();
            let event = if proxy_root.is_some() {
                RuffleEvent::SetSeer2ProxyRoot(None)
            } else {
                RuffleEvent::BrowseSeer2ProxyRoot
            };
            let _ = self.event_loop.send_event(event);
        }
    }

    fn show_clear_file_cache_confirmation(
        &mut self,
        locale: &LanguageIdentifier,
        context: &egui::Context,
    ) {
        if !self.confirm_clear_file_cache {
            return;
        }

        let request_no_focus = std::mem::take(&mut self.focus_clear_file_cache_no);
        let mut keep_open = true;
        let mut confirmed = false;
        let mut rejected = false;
        egui::Window::new(text(locale, "cache-metrics-confirm-title"))
            .open(&mut keep_open)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(context, |ui| {
                ui.label(text(locale, "cache-metrics-confirm-body"));
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let no = ui.button(text(locale, "cache-metrics-confirm-no"));
                    if request_no_focus {
                        no.request_focus();
                    }
                    rejected = no.clicked();
                    confirmed = ui
                        .button(text(locale, "cache-metrics-confirm-yes"))
                        .clicked();
                });
            });

        rejected |= context.input(|input| input.key_pressed(Key::Escape));
        if confirmed {
            self.confirm_clear_file_cache = false;
            self.clear_file_cache(locale);
        } else if rejected || !keep_open {
            self.confirm_clear_file_cache = false;
        }
    }

    fn clear_file_cache(&self, locale: &LanguageIdentifier) {
        let options = self
            .currently_opened
            .as_ref()
            .map(|(_, options)| options)
            .unwrap_or(&self.default_launch_options);
        let directory = options
            .seer2_cache_directory
            .clone()
            .unwrap_or_else(|| options.cache_directory.join("seer2"));
        match clear_seer2_file_cache(&directory) {
            Ok(result) if result.failed_files == 0 => tracing::info!(
                "Cleared {} Seer2 file-cache entries ({} bytes) from {}",
                result.removed_files,
                result.removed_bytes,
                directory.display()
            ),
            Ok(result) => {
                let error = format!(
                    "Removed {} cache files, but {} files could not be removed.",
                    result.removed_files, result.failed_files
                );
                tracing::warn!("{error}");
                self.show_cache_error(locale, &error);
            }
            Err(error) => {
                tracing::error!(
                    "Failed to clear Seer2 file cache {}: {error}",
                    directory.display()
                );
                self.show_cache_error(locale, &error.to_string());
            }
        }
    }

    fn show_cache_error(&self, locale: &LanguageIdentifier, error: &str) {
        MessageDialog::new()
            .set_level(MessageLevel::Error)
            .set_title(text(locale, "cache-metrics-error-title"))
            .set_description(error)
            .set_buttons(MessageButtons::Ok)
            .show();
    }

    fn file_menu(
        &mut self,
        locale: &LanguageIdentifier,
        ui: &mut egui::Ui,
        dialogs: &mut Dialogs,
        player_exists: bool,
    ) {
        ui.menu_button(text(locale, "file-menu"), |ui| {
            if Button::new(text(locale, "file-menu-game-home"))
                .ui(ui)
                .clicked()
            {
                ui.close();
                self.open_game_home();
            }
            ui.separator();

            if Button::new(text(locale, "file-menu-open-file"))
                .shortcut_text(ui.ctx().format_shortcut(&Self::SHORTCUT_OPEN))
                .ui(ui)
                .clicked()
            {
                ui.close();
                self.browse_and_open(OpenType::File);
            }

            if Button::new(text(locale, "file-menu-open-directory"))
                .ui(ui)
                .clicked()
            {
                ui.close();
                self.browse_and_open(OpenType::Directory);
            }

            if Button::new(text(locale, "file-menu-open-advanced"))
                .shortcut_text(ui.ctx().format_shortcut(&Self::SHORTCUT_OPEN_ADVANCED))
                .ui(ui)
                .clicked()
            {
                ui.close();
                dialogs.open_file_advanced();
            }
            ui.separator();

            if ui
                .add_enabled(player_exists, Button::new(text(locale, "file-menu-reload")))
                .clicked()
            {
                self.reload_movie(ui);
            }

            if ui
                .add_enabled(player_exists, Button::new(text(locale, "file-menu-close")))
                .clicked()
            {
                self.close_movie(ui);
            }
            ui.separator();

            let recent_menu_response = ui
                .menu_button(text(locale, "file-menu-recents"), |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    ui.set_min_width(250.0);

                    if self
                        .cached_recents
                        .as_ref()
                        .map(|x| x.is_empty())
                        .unwrap_or(true)
                    {
                        ui.label(text(locale, "file-menu-recents-empty"));
                    }

                    if let Some(recents) = &self.cached_recents {
                        for recent in recents {
                            if ui.button(&recent.name).clicked() {
                                ui.close();
                                let _ = self.event_loop.send_event(RuffleEvent::Open(
                                    recent.content_descriptor.clone(),
                                    Box::new(self.default_launch_options.clone()),
                                ));
                            }
                        }
                    };
                })
                .inner;

            match recent_menu_response {
                // recreate the cache on the first draw.
                Some(_) if self.cached_recents.is_none() => {
                    self.cached_recents = Some(self.preferences.recents(|recents| {
                        recents
                            .iter()
                            .rev()
                            .filter(|x| !x.is_invalid() && x.is_available())
                            .cloned()
                            .collect::<Vec<_>>()
                    }))
                }
                // clear cache, since menu was closed.
                None if self.cached_recents.is_some() => self.cached_recents = None,
                _ => {}
            }
            ui.separator();

            if ui
                .add_enabled(player_exists, Button::new(text(locale, "file-menu-export")))
                .clicked()
            {
                self.export_bundle(ui);
            }
            ui.separator();

            if Button::new(text(locale, "file-menu-preferences"))
                .ui(ui)
                .clicked()
            {
                ui.close();
                dialogs.open_preferences();
            }
            ui.separator();

            if Button::new(text(locale, "file-menu-exit"))
                .shortcut_text(ui.ctx().format_shortcut(&Self::SHORTCUT_QUIT))
                .ui(ui)
                .clicked()
            {
                ui.close();
                self.request_exit();
            }
        });
    }

    fn view_menu(
        &self,
        locale: &LanguageIdentifier,
        ui: &mut egui::Ui,
        player: &mut Option<&mut Player>,
    ) {
        ui.menu_button(text(locale, "view-menu"), |ui| {
            ui.add_enabled_ui(player.is_some(), |ui| {
                ui.menu_button(text(locale, "scale-mode"), |ui| {
                    let items = [
                        (
                            "scale-mode-noscale",
                            "scale-mode-noscale-tooltip",
                            StageScaleMode::NoScale,
                        ),
                        (
                            "scale-mode-showall",
                            "scale-mode-showall-tooltip",
                            StageScaleMode::ShowAll,
                        ),
                        (
                            "scale-mode-exactfit",
                            "scale-mode-exactfit-tooltip",
                            StageScaleMode::ExactFit,
                        ),
                        (
                            "scale-mode-noborder",
                            "scale-mode-noborder-tooltip",
                            StageScaleMode::NoBorder,
                        ),
                    ];
                    let current_scale_mode = player.as_mut().map(|player| player.scale_mode());
                    for (id, tooltip_id, scale_mode) in items {
                        let response = if Some(scale_mode) == current_scale_mode {
                            ui.checkbox(&mut true, text(locale, id))
                        } else {
                            ui.button(text(locale, id))
                        }
                        .on_hover_text_at_pointer(text(locale, tooltip_id));
                        if response.clicked() {
                            ui.close();
                            if let Some(player) = player {
                                player.set_scale_mode(scale_mode);
                            }
                        }
                    }
                    ui.separator();

                    let original_forced_scale_mode = player
                        .as_mut()
                        .map(|player| player.forced_scale_mode())
                        .unwrap_or_default();
                    let mut forced_scale_mode = original_forced_scale_mode;
                    ui.checkbox(&mut forced_scale_mode, text(locale, "scale-mode-force"))
                        .on_hover_text_at_pointer(text(locale, "scale-mode-force-tooltip"));
                    if let Some(player) = player
                        && forced_scale_mode != original_forced_scale_mode
                    {
                        player.set_forced_scale_mode(forced_scale_mode);
                    }
                });

                let original_letterbox = if let Some(player) = player {
                    player.letterbox() == Letterbox::On
                } else {
                    false
                };
                let mut letterbox = original_letterbox;
                ui.checkbox(&mut letterbox, text(locale, "letterbox"));
                if let Some(player) = player
                    && letterbox != original_letterbox
                {
                    player.set_letterbox(if letterbox {
                        Letterbox::On
                    } else {
                        Letterbox::Off
                    });
                }
                ui.separator();

                if Button::new(text(locale, "view-menu-fullscreen"))
                    .shortcut_text(ui.ctx().format_shortcut(&Self::SHORTCUT_FULLSCREEN))
                    .ui(ui)
                    .clicked()
                {
                    ui.close();
                    if let Some(player) = player {
                        player.set_fullscreen(true);
                    }
                }
                ui.separator();

                ui.menu_button(text(locale, "quality"), |ui| {
                    let items = [
                        ("quality-low", StageQuality::Low),
                        ("quality-medium", StageQuality::Medium),
                        ("quality-high", StageQuality::High),
                        ("quality-best", StageQuality::Best),
                        ("quality-high8x8", StageQuality::High8x8),
                        ("quality-high8x8linear", StageQuality::High8x8Linear),
                        ("quality-high16x16", StageQuality::High16x16),
                        ("quality-high16x16linear", StageQuality::High16x16Linear),
                    ];
                    let current_quality = player.as_mut().map(|player| player.quality());
                    for (id, quality) in items {
                        let clicked = if Some(quality) == current_quality {
                            ui.checkbox(&mut true, text(locale, id)).clicked()
                        } else {
                            ui.button(text(locale, id)).clicked()
                        };
                        if clicked {
                            ui.close();
                            if let Some(player) = player {
                                player.set_quality(quality);
                            }
                        }
                    }
                });
            });
        });
    }

    fn controls_menu(
        &self,
        locale: &LanguageIdentifier,
        ui: &mut egui::Ui,
        dialogs: &mut Dialogs,
        player: &mut Option<&mut Player>,
    ) {
        ui.menu_button(text(locale, "controls-menu"), |ui| {
            ui.add_enabled_ui(player.is_some(), |ui| {
                let playing = player.as_ref().map(|p| p.is_playing()).unwrap_or_default();
                let btn_name = if playing {
                    "controls-menu-suspend"
                } else {
                    "controls-menu-resume"
                };
                if Button::new(text(locale, btn_name))
                    .shortcut_text(ui.ctx().format_shortcut(&Self::SHORTCUT_PAUSE))
                    .ui(ui)
                    .clicked()
                {
                    ui.close();
                    if let Some(player) = player {
                        player.set_is_playing(!playing);
                    }
                }

                ui.add_enabled_ui(!playing, |ui| {
                    if Button::new(text(locale, "controls-menu-step-once"))
                        .shortcut_text(ui.ctx().format_shortcut(&Self::SHORTCUT_STEP))
                        .ui(ui)
                        .clicked()
                    {
                        ui.close();
                        if let Some(player) = player {
                            player.suspend_after_next_frame();
                        }
                    }
                });
            });
            if Button::new(text(locale, "controls-menu-volume"))
                .ui(ui)
                .clicked()
            {
                dialogs.open_volume_controls();
                ui.close();
            }
        });
    }

    fn browse_and_open(&self, open_type: OpenType) {
        let _ = self.event_loop.send_event(RuffleEvent::BrowseAndOpen(
            Box::new(self.default_launch_options.clone()),
            open_type,
        ));
    }

    fn open_game_home(&self) {
        let mut options = self.default_launch_options.clone();
        options.seer2_virtual_http = true;
        let url = Url::parse(SEER2_GAME_HOME_URL).expect("Seer2 game home URL should be valid");
        let _ = self.event_loop.send_event(RuffleEvent::Open(
            ContentDescriptor::new_remote(url),
            Box::new(options),
        ));
    }

    fn close_movie(&mut self, ui: &egui::Ui) {
        let _ = self.event_loop.send_event(RuffleEvent::CloseFile);
        self.currently_opened = None;
        ui.close();
    }

    fn reload_movie(&mut self, ui: &egui::Ui) {
        let _ = self.event_loop.send_event(RuffleEvent::CloseFile);
        if let Some((movie_url, opts)) = self.currently_opened.take() {
            let _ = self
                .event_loop
                .send_event(RuffleEvent::Open(movie_url, opts.into()));
        }
        ui.close();
    }

    fn request_exit(&self) {
        let _ = self.event_loop.send_event(RuffleEvent::ExitRequested);
    }

    fn launch_website(&self, ui: &egui::Ui, url: &str) {
        let _ = webbrowser::open(url);
        ui.close();
    }

    fn export_bundle(&self, ui: &egui::Ui) {
        let _ = self.event_loop.send_event(RuffleEvent::ExportBundle);
        ui.close();
    }
}
