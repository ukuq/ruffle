use crate::gui::ThemePreference;
use crate::gui::controller::load_system_fonts;
use crate::gui::text;
use anyhow::{Context as _, Result};
use chrono::{DateTime, Local, Utc};
use egui::{Color32, Label, Sense, TextWrapMode, ViewportId};
use egui_extras::{Column, TableBuilder};
use ruffle_frontend_utils::backends::navigator::{
    NetworkRequestRecord, NetworkRequestSource, seer2_network_monitor,
};
use ruffle_render_wgpu::descriptors::Descriptors;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unic_langid::LanguageIdentifier;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Theme, Window, WindowAttributes, WindowId};

const REFRESH_INTERVAL: Duration = Duration::from_millis(250);

/// A standalone native window that displays Seer2 virtual HTTP traffic.
pub(crate) struct NetworkMonitorWindow {
    window: Arc<Window>,
    descriptors: Arc<Descriptors>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    locale: LanguageIdentifier,
    theme_preference: ThemePreference,
    view: NetworkMonitorView,
    next_refresh: Instant,
}

impl NetworkMonitorWindow {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        descriptors: Arc<Descriptors>,
        font_database: &fontdb::Database,
        locale: LanguageIdentifier,
        theme_preference: ThemePreference,
    ) -> Result<Self> {
        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title(text(&locale, "network-monitor-title"))
                        .with_inner_size(LogicalSize::new(1_050, 560))
                        .with_min_inner_size(LogicalSize::new(620, 320)),
                )
                .context("failed to create the network monitor window")?,
        );
        let surface = descriptors
            .wgpu_instance
            .create_surface(window.clone())
            .context("failed to create the network monitor surface")?;
        let capabilities = surface.get_capabilities(&descriptors.adapter);
        let surface_format = [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8Unorm,
        ]
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
        .or_else(|| capabilities.formats.first().copied())
        .context("the network monitor surface has no supported formats")?;
        let size = window.inner_size();
        configure_surface(&surface, &descriptors, surface_format, size);

        let egui_context = egui::Context::default();
        egui_context.set_fonts(load_system_fonts(font_database, locale.clone()));
        apply_theme(&egui_context, window.theme(), theme_preference);
        egui_extras::install_image_loaders(&egui_context);
        let mut egui_winit = egui_winit::State::new(
            egui_context,
            ViewportId::ROOT,
            window.as_ref(),
            None,
            None,
            None,
        );
        egui_winit.set_max_texture_side(descriptors.limits.max_texture_dimension_2d as usize);
        let egui_renderer = egui_wgpu::Renderer::new(
            &descriptors.device,
            surface_format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );

        window.request_redraw();
        Ok(Self {
            window,
            descriptors,
            surface,
            surface_format,
            size,
            egui_winit,
            egui_renderer,
            locale,
            theme_preference,
            view: NetworkMonitorView::new(),
            next_refresh: Instant::now() + REFRESH_INTERVAL,
        })
    }

    pub(crate) fn id(&self) -> WindowId {
        self.window.id()
    }

    pub(crate) fn focus(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.focus_window();
        self.window.request_redraw();
    }

    pub(crate) fn window_event(&mut self, event: &WindowEvent) {
        if let WindowEvent::RedrawRequested = event {
            self.render();
            return;
        }

        if let WindowEvent::Resized(size) = event
            && size.width > 0
            && size.height > 0
        {
            self.size = *size;
            configure_surface(
                &self.surface,
                &self.descriptors,
                self.surface_format,
                self.size,
            );
        }
        if let WindowEvent::ThemeChanged(theme) = event {
            apply_theme(
                self.egui_winit.egui_ctx(),
                Some(*theme),
                self.theme_preference,
            );
        }

        let response = self.egui_winit.on_window_event(&self.window, event);
        if response.repaint {
            self.window.request_redraw();
        }
    }

    /// Refresh periodically so requests completed in the player appear without
    /// requiring mouse movement inside this window.
    pub(crate) fn about_to_wait(&mut self) -> Instant {
        let now = Instant::now();
        if now >= self.next_refresh {
            self.window.request_redraw();
            self.next_refresh = now + REFRESH_INTERVAL;
        }
        self.next_refresh
    }

    fn render(&mut self) {
        if self.size.width == 0 || self.size.height == 0 {
            return;
        }

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                configure_surface(
                    &self.surface,
                    &self.descriptors,
                    self.surface_format,
                    self.size,
                );
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return,
            wgpu::CurrentSurfaceTexture::Validation => {
                tracing::error!("Network monitor surface failed validation");
                return;
            }
        };

        let raw_input = self.egui_winit.take_egui_input(&self.window);
        let context = self.egui_winit.egui_ctx().clone();
        let locale = &self.locale;
        let view = &mut self.view;
        let mut full_output = context.run_ui(raw_input, |ui| {
            ui.painter()
                .rect_filled(ui.max_rect(), 0.0, ui.visuals().panel_fill);
            view.show(locale, ui);
        });
        self.egui_winit
            .handle_platform_output(&self.window, full_output.platform_output);
        let clipped_primitives =
            context.tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point: self.window.scale_factor() as f32,
        };
        let mut encoder =
            self.descriptors
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("network monitor egui encoder"),
                });

        for (id, image_deltas) in full_output.textures_delta.set.drain() {
            for image_delta in &image_deltas {
                self.egui_renderer.update_texture(
                    &self.descriptors.device,
                    &self.descriptors.queue,
                    id,
                    image_delta,
                );
            }
        }
        let mut command_buffers = self.egui_renderer.update_buffers(
            &self.descriptors.device,
            &self.descriptors.queue,
            &mut encoder,
            &clipped_primitives,
            &screen_descriptor,
        );
        {
            let surface_view = surface_texture.texture.create_view(&Default::default());
            let mut render_pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    label: Some("network_monitor_egui_render"),
                    ..Default::default()
                })
                .forget_lifetime();
            self.egui_renderer
                .render(&mut render_pass, &clipped_primitives, &screen_descriptor);
        }
        command_buffers.push(encoder.finish());
        self.descriptors.queue.submit(command_buffers);
        for id in full_output.textures_delta.free.drain() {
            self.egui_renderer.free_texture(&id);
        }
        self.window.pre_present_notify();
        self.descriptors.queue.present(surface_texture);
        self.next_refresh = Instant::now() + REFRESH_INTERVAL;
    }
}

fn configure_surface(
    surface: &wgpu::Surface<'_>,
    descriptors: &Descriptors,
    format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
) {
    surface.configure(
        &descriptors.device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: Default::default(),
            desired_maximum_frame_latency: 2,
            alpha_mode: Default::default(),
            view_formats: Default::default(),
        },
    );
}

fn apply_theme(context: &egui::Context, system_theme: Option<Theme>, preference: ThemePreference) {
    let theme = match preference {
        ThemePreference::Light => Some(Theme::Light),
        ThemePreference::Dark => Some(Theme::Dark),
        ThemePreference::System => system_theme,
    };
    if let Some(theme) = theme {
        context.set_theme(match theme {
            Theme::Light => egui::Theme::Light,
            Theme::Dark => egui::Theme::Dark,
        });
    }
}

struct NetworkMonitorView {
    filter: String,
    selected_request: Option<u64>,
}

impl NetworkMonitorView {
    fn new() -> Self {
        Self {
            filter: String::new(),
            selected_request: None,
        }
    }

    fn show(&mut self, locale: &LanguageIdentifier, ui: &mut egui::Ui) {
        let records = seer2_network_monitor().snapshot();
        let filter = self.filter.trim().to_lowercase();
        let visible: Vec<_> = records
            .iter()
            .filter(|record| request_matches(record, &filter))
            .collect();
        let pending = records
            .iter()
            .filter(|record| record.source == NetworkRequestSource::Pending)
            .count();
        let failed = records
            .iter()
            .filter(|record| {
                record.source == NetworkRequestSource::Error
                    || record.status.is_some_and(|status| status >= 400)
            })
            .count();

        ui.horizontal(|ui| {
            if ui.button(text(locale, "network-monitor-clear")).clicked() {
                seer2_network_monitor().clear();
                self.selected_request = None;
            }
            ui.label(text(locale, "network-monitor-filter"));
            ui.text_edit_singleline(&mut self.filter);
            ui.separator();
            ui.label(format!(
                "{} / {}  |  {}: {}  |  {}: {}",
                visible.len(),
                records.len(),
                text(locale, "network-monitor-pending"),
                pending,
                text(locale, "network-monitor-failed"),
                failed
            ));
        });
        ui.separator();

        let row_height = egui::TextStyle::Body
            .resolve(ui.style())
            .size
            .max(ui.spacing().interact_size.y);
        let table_height = if self.selected_request.is_some() {
            (ui.available_height() - 145.0).max(120.0)
        } else {
            ui.available_height()
        };

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .max_scroll_height(table_height)
            .column(Column::exact(56.0))
            .column(Column::exact(58.0))
            .column(Column::exact(82.0))
            .column(Column::exact(92.0))
            .column(Column::exact(74.0))
            .column(Column::remainder())
            .header(22.0, |mut header| {
                table_header(&mut header, locale, "network-monitor-method");
                table_header(&mut header, locale, "network-monitor-status");
                table_header(&mut header, locale, "network-monitor-source");
                table_header(&mut header, locale, "network-monitor-time");
                table_header(&mut header, locale, "network-monitor-size");
                table_header(&mut header, locale, "network-monitor-url");
            })
            .body(|mut body| {
                for record in visible.iter().rev() {
                    body.row(row_height, |mut row| {
                        row.set_selected(self.selected_request == Some(record.id));
                        row.col(|ui| {
                            ui.label(&record.method);
                        });
                        row.col(|ui| {
                            let status = record
                                .status
                                .map_or_else(|| "-".to_string(), |value| value.to_string());
                            ui.colored_label(status_color(record), status);
                        });
                        row.col(|ui| {
                            ui.label(record.source.as_str());
                        });
                        row.col(|ui| {
                            ui.label(format_duration(record.duration_millis));
                        });
                        row.col(|ui| {
                            ui.label(format_bytes(record.response_bytes));
                        });
                        row.col(|ui| {
                            ui.add(
                                Label::new(&record.url)
                                    .selectable(false)
                                    .wrap_mode(TextWrapMode::Truncate),
                            )
                            .on_hover_text(&record.url);
                        });
                        if row.response().clicked() {
                            self.selected_request = Some(record.id);
                        }
                    });
                }
            });

        if let Some(record) = self
            .selected_request
            .and_then(|id| records.iter().find(|record| record.id == id))
        {
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(125.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(text(locale, "network-monitor-started"));
                        ui.label(format_started_at(record.started_at_millis));
                        ui.separator();
                        ui.strong(text(locale, "network-monitor-duration"));
                        ui.label(format_duration(record.duration_millis));
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(text(locale, "network-monitor-url"));
                        ui.monospace(&record.url);
                    });
                    if let Some(upstream) = &record.upstream_url {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong(text(locale, "network-monitor-upstream"));
                            ui.monospace(upstream);
                        });
                    }
                    if let Some(error) = &record.error {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong(text(locale, "network-monitor-error"));
                            ui.colored_label(Color32::RED, error);
                        });
                    }
                });
        }
    }
}

fn table_header(
    header: &mut egui_extras::TableRow<'_, '_>,
    locale: &LanguageIdentifier,
    label: &'static str,
) {
    header.col(|ui| {
        ui.strong(text(locale, label));
    });
}

fn request_matches(record: &NetworkRequestRecord, filter: &str) -> bool {
    filter.is_empty()
        || record.url.to_lowercase().contains(filter)
        || record.method.to_lowercase().contains(filter)
        || record.source.as_str().to_lowercase().contains(filter)
        || record
            .status
            .is_some_and(|status| status.to_string().contains(filter))
        || record
            .upstream_url
            .as_ref()
            .is_some_and(|url| url.to_lowercase().contains(filter))
}

fn status_color(record: &NetworkRequestRecord) -> Color32 {
    match record.status {
        Some(200..=299) => Color32::from_rgb(80, 190, 110),
        Some(400..) | None if record.source == NetworkRequestSource::Error => Color32::RED,
        Some(300..=399) => Color32::YELLOW,
        Some(400..) => Color32::RED,
        _ => Color32::GRAY,
    }
}

fn format_duration(duration_millis: Option<u128>) -> String {
    duration_millis.map_or_else(
        || "Pending".to_string(),
        |duration| {
            if duration < 1_000 {
                format!("{duration} ms")
            } else {
                format!("{:.2} s", duration as f64 / 1_000.0)
            }
        },
    )
}

fn format_bytes(bytes: Option<usize>) -> String {
    bytes.map_or_else(
        || "-".to_string(),
        |bytes| {
            if bytes < 1_024 {
                format!("{bytes} B")
            } else if bytes < 1024 * 1024 {
                format!("{:.1} KiB", bytes as f64 / 1024.0)
            } else {
                format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
            }
        },
    )
}

fn format_started_at(started_at_millis: u128) -> String {
    i64::try_from(started_at_millis)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}
