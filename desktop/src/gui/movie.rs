use crate::gui::MENU_HEIGHT;
use ruffle_render_wgpu::descriptors::Descriptors;
use ruffle_render_wgpu::target::{RenderTarget, RenderTargetFrame};
use std::borrow::Cow;
use std::sync::Arc;
use wgpu::util::DeviceExt;

#[derive(Debug)]
pub struct MovieViewRenderer {
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    vertices: wgpu::Buffer,
}

#[derive(Clone, Copy, Debug)]
pub struct MovieViewLayout {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl MovieViewLayout {
    pub fn right(self) -> f64 {
        self.left + self.width
    }

    pub fn bottom(self) -> f64 {
        self.top + self.height
    }
}

pub fn get_movie_view_layout(
    has_menu: bool,
    window_width: u32,
    window_height: u32,
    scale_factor: f64,
    movie_width: u32,
    movie_height: u32,
) -> MovieViewLayout {
    let window_width = f64::from(window_width.max(1));
    let window_height = f64::from(window_height.max(1));
    let movie_width = f64::from(movie_width.max(1));
    let movie_height = f64::from(movie_height.max(1));

    let menu_height = if has_menu {
        (MENU_HEIGHT as f64 * scale_factor).min(window_height)
    } else {
        0.0
    };
    let content_top = menu_height;
    let content_height = (window_height - content_top).max(1.0);

    let movie_aspect = movie_width / movie_height;
    let content_aspect = window_width / content_height;
    let (draw_width, draw_height) = if content_aspect > movie_aspect {
        let draw_height = content_height;
        (draw_height * movie_aspect, draw_height)
    } else {
        let draw_width = window_width;
        (draw_width, draw_width / movie_aspect)
    };

    MovieViewLayout {
        left: (window_width - draw_width) / 2.0,
        top: content_top + (content_height - draw_height) / 2.0,
        width: draw_width,
        height: draw_height,
    }
}

fn get_vertices(
    has_menu: bool,
    window_width: u32,
    window_height: u32,
    scale_factor: f64,
    movie_width: u32,
    movie_height: u32,
) -> [[f32; 4]; 6] {
    let layout = get_movie_view_layout(
        has_menu,
        window_width,
        window_height,
        scale_factor,
        movie_width,
        movie_height,
    );
    let window_width = f64::from(window_width.max(1));
    let window_height = f64::from(window_height.max(1));

    let left = ((layout.left / window_width) * 2.0 - 1.0) as f32;
    let right = ((layout.right() / window_width) * 2.0 - 1.0) as f32;
    let top = (1.0 - (layout.top / window_height) * 2.0) as f32;
    let bottom = (1.0 - (layout.bottom() / window_height) * 2.0) as f32;

    // x y u v
    [
        [left, top, 0.0, 0.0],     // tl
        [right, top, 1.0, 0.0],    // tr
        [right, bottom, 1.0, 1.0], // br
        [right, bottom, 1.0, 1.0], // br
        [left, bottom, 0.0, 1.0],  // bl
        [left, top, 0.0, 0.0],     // tl
    ]
}

impl MovieViewRenderer {
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        has_menu: bool,
        width: u32,
        height: u32,
        scale_factor: f64,
        movie_width: u32,
        movie_height: u32,
    ) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("blit.wgsl"))),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                entry_point: Some("vs_main"),
                module: &module,
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 4 * 4,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    // 0: vec2 position
                    // 1: vec2 texture coordinates
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                })],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                unclipped_depth: false,
                conservative: false,
                cull_mode: None,
                front_face: wgpu::FrontFace::default(),
                polygon_mode: wgpu::PolygonMode::default(),
                strip_index_format: None,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                alpha_to_coverage_enabled: false,
                count: 1,
                mask: !0,
            },

            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(if surface_format.is_srgb() {
                    "fs_main_srgb_framebuffer"
                } else {
                    "fs_main_linear_framebuffer"
                }),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&get_vertices(
                has_menu,
                width,
                height,
                scale_factor,
                movie_width,
                movie_height,
            )),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });

        Self {
            bind_group_layout,
            pipeline,
            sampler,
            vertices,
        }
    }

    pub fn update_resolution(
        &self,
        descriptors: &Descriptors,
        has_menu: bool,
        width: u32,
        height: u32,
        scale_factor: f64,
        movie_width: u32,
        movie_height: u32,
    ) {
        descriptors.queue.write_buffer(
            &self.vertices,
            0,
            bytemuck::cast_slice(&get_vertices(
                has_menu,
                width,
                height,
                scale_factor,
                movie_width,
                movie_height,
            )),
        );
    }
}

#[derive(Debug)]
pub struct MovieView {
    renderer: Arc<MovieViewRenderer>,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    #[cfg(feature = "tracy_images")]
    tracy_frame_captures: crate::tracy::FrameCapturesHolder,
}

impl MovieView {
    pub fn new(
        renderer: Arc<MovieViewRenderer>,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        #[cfg(feature = "tracy_images")] tracy_frame_captures: crate::tracy::FrameCapturesHolder,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &renderer.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&renderer.sampler),
                },
            ],
        });
        #[cfg(feature = "tracy_images")]
        tracy_frame_captures.set_target(device, Some(&texture));
        Self {
            renderer,
            texture,
            bind_group,
            #[cfg(feature = "tracy_images")]
            tracy_frame_captures,
        }
    }

    pub fn render(
        &self,
        renderer: &MovieViewRenderer,
        render_pass: &mut wgpu::RenderPass<'static>,
    ) {
        render_pass.set_pipeline(&renderer.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, renderer.vertices.slice(..));
        render_pass.draw(0..6, 0..1);
    }

    pub fn width(&self) -> u32 {
        self.texture.width()
    }

    pub fn height(&self) -> u32 {
        self.texture.height()
    }
}

impl RenderTarget for MovieView {
    type Frame = MovieViewFrame;

    fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        *self = MovieView::new(
            self.renderer.clone(),
            device,
            width,
            height,
            #[cfg(feature = "tracy_images")]
            self.tracy_frame_captures.clone(),
        );
    }

    fn format(&self) -> wgpu::TextureFormat {
        self.texture.format()
    }

    fn width(&self) -> u32 {
        self.texture.width()
    }

    fn height(&self) -> u32 {
        self.texture.height()
    }

    fn get_next_texture(&mut self) -> Option<Self::Frame> {
        Some(MovieViewFrame(
            self.texture.create_view(&Default::default()),
        ))
    }

    fn submit<I: IntoIterator<Item = wgpu::CommandBuffer>>(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        command_buffers: I,
        _frame: Self::Frame,
    ) -> wgpu::SubmissionIndex {
        queue.submit(command_buffers)
    }
}

#[derive(Debug)]
pub struct MovieViewFrame(wgpu::TextureView);

impl RenderTargetFrame for MovieViewFrame {
    fn into_view(self) -> wgpu::TextureView {
        self.0
    }

    fn view(&self) -> &wgpu::TextureView {
        &self.0
    }
}
