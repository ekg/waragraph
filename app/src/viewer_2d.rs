use crate::annotations::AnnotationStore;
use crate::app::AppWindow;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use wgpu::BufferUsages;
use winit::event::WindowEvent;
use winit::event_loop::{EventLoop, EventLoopWindowTarget};
use winit::window::Window;

use raving_wgpu::camera::DynamicCamera2d;
use raving_wgpu::graph::dfrog::{Graph, InputResource};
use raving_wgpu::gui::EguiCtx;
use raving_wgpu::{NodeId, State, WindowState};

use wgpu::util::{BufferInitDescriptor, DeviceExt};

use anyhow::Result;

use ultraviolet::*;

use waragraph_core::graph::PathIndex;

pub mod layout;

use layout::{GraphPathCurves, NodePositions, PathCurveBuffers};

#[derive(Debug)]
pub struct Args {
    pub gfa: PathBuf,
    pub tsv: PathBuf,
    pub annotations: Option<PathBuf>,
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct GpuVertex {
    pos: [f32; 2],
    // tex_coord: [f32; 2],
}

pub struct Viewer2D {
    path_index: Arc<PathIndex>,

    node_positions: Arc<NodePositions>,
    vertex_buffer: wgpu::Buffer,
    instance_count: usize,

    camera: DynamicCamera2d,

    transform_uniform: wgpu::Buffer,
    vert_config: wgpu::Buffer,

    render_graph: Graph,
    draw_node: NodeId,
}

impl Viewer2D {
    pub fn init(
        state: &State,
        window: &WindowState,
        path_index: Arc<PathIndex>,
        layout_tsv: impl AsRef<std::path::Path>,
    ) -> Result<Self> {
        let (node_positions, vertex_buffer, instance_count) = {
            let pos = NodePositions::from_layout_tsv(layout_tsv)?;

            let vertex_data = pos.iter_nodes().collect::<Vec<_>>();

            let instance_count = vertex_data.len() / 2;

            let buffer = state.device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("Viewer2D Vertex Buffer"),
                    contents: bytemuck::cast_slice(&vertex_data),
                    usage: wgpu::BufferUsages::VERTEX,
                },
            );

            (pos, buffer, instance_count)
        };

        let win_dims = {
            let [w, h]: [u32; 2] = window.window.inner_size().into();
            Vec2::new(w as f32, h as f32)
        };

        let (tl, br) = node_positions.bounds;
        let center = tl + 0.5 * (br - tl);
        let total_size = br - tl;

        let aspect = win_dims.x / win_dims.y;

        let cam_width = total_size.y * aspect;
        let size = Vec2::new(cam_width, total_size.y);

        let camera = DynamicCamera2d::new(center, size);

        let mut graph = Graph::new();

        let draw_node_schema = {
            let vert_src = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/shaders/2d_rects.vert.spv"
            ));
            let frag_src = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/shaders/uv_rg.frag.spv"
            ));

            let primitive = wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Cw,
                cull_mode: None, // TODO fix
                // cull_mode: Some(wgpu::Face::Front),
                polygon_mode: wgpu::PolygonMode::Fill,

                strip_index_format: None,
                unclipped_depth: false,
                conservative: false,
            };

            graph.add_graphics_schema_custom(
                state,
                vert_src,
                frag_src,
                primitive,
                wgpu::VertexStepMode::Instance,
                ["vertex_in"],
                None,
                &[window.surface_format],
            )?
        };

        let (transform_uniform, vert_config) = {
            let usage = BufferUsages::UNIFORM | BufferUsages::COPY_DST;

            let data = camera.to_matrix();

            let transform =
                state.device.create_buffer_init(&BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::cast_slice(&[data]),
                    usage,
                });

            let data = [20.0f32, 0.0, 0.0, 0.0];

            let vert_config =
                state.device.create_buffer_init(&BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::cast_slice(&[data]),
                    usage,
                });

            (transform, vert_config)
        };

        let draw_node = graph.add_node(draw_node_schema);

        graph.add_link_from_transient("vertices", draw_node, 0);
        graph.add_link_from_transient("swapchain", draw_node, 1);

        graph.add_link_from_transient("transform", draw_node, 2);
        graph.add_link_from_transient("vert_cfg", draw_node, 3);

        let instances = instance_count as u32;

        graph.set_node_preprocess_fn(draw_node, move |_ctx, op_state| {
            op_state.vertices = Some(0..6);
            op_state.instances = Some(0..instances);
        });

        Ok(Self {
            path_index,
            node_positions: Arc::new(node_positions),

            vertex_buffer,
            instance_count,

            camera,

            transform_uniform,
            vert_config,

            render_graph: graph,
            draw_node,
        })
    }
}

impl AppWindow for Viewer2D {
    fn update(
        &mut self,
        tokio_handle: &tokio::runtime::Handle,
        state: &raving_wgpu::State,
        window: &raving_wgpu::WindowState,
        egui_ctx: &mut EguiCtx,
        dt: f32,
    ) {
        let [width, height]: [u32; 2] = window.window.inner_size().into();
        let dims = ultraviolet::Vec2::new(width as f32, height as f32);

        let screen_rect = egui::Rect::from_min_max(
            egui::pos2(0.0, 0.0),
            egui::pos2(dims.x, dims.y),
        );

        egui_ctx.begin_frame(&window.window);

        egui_ctx.end_frame(&window.window);

        self.camera.update(dt);
    }

    fn on_event(
        &mut self,
        window_dims: [u32; 2],
        event: &winit::event::WindowEvent,
    ) -> bool {
        let mut consume = false;

        if let WindowEvent::KeyboardInput { input, .. } = event {
            if let Some(key) = input.virtual_keycode {
                use winit::event::ElementState;
                use winit::event::VirtualKeyCode as Key;
                let pressed = matches!(input.state, ElementState::Pressed);

                if pressed {
                    match key {
                        Key::Right => {
                            // self.view.translate_norm_f32(0.1);
                        }
                        Key::Left => {
                            // self.view.translate_norm_f32(-0.1);
                        }
                        Key::Up => {
                            // self.path_list_view.scroll_relative(-1);
                            // self.force_resample = true;
                        }
                        Key::Down => {
                            // self.path_list_view.scroll_relative(1);
                            // self.force_resample = true;
                        }
                        Key::Space => {
                            // self.view.reset();
                        }
                        _ => (),
                    }
                }

                // self.view = l..r;
            }
        }

        consume
    }

    fn on_resize(
        &mut self,
        state: &raving_wgpu::State,
        old_window_dims: [u32; 2],
        new_window_dims: [u32; 2],
    ) -> anyhow::Result<()> {
        // TODO *maybe* update view here, but might not be necessary
        Ok(())
    }

    fn render(
        &mut self,
        state: &raving_wgpu::State,
        window: &WindowState,
        swapchain_view: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
    ) -> anyhow::Result<()> {
        let size: [u32; 2] = window.window.inner_size().into();

        let mut transient_res: HashMap<String, InputResource<'_>> =
            HashMap::default();

        let format = window.surface_format;

        transient_res.insert(
            "swapchain".into(),
            InputResource::Texture {
                size,
                format,
                texture: None,
                view: Some(&swapchain_view),
                sampler: None,
            },
        );

        let v_stride = std::mem::size_of::<[f32; 4]>();
        transient_res.insert(
            "vertices".into(),
            InputResource::Buffer {
                size: self.instance_count * v_stride,
                stride: Some(v_stride),
                buffer: &self.vertex_buffer,
            },
        );

        transient_res.insert(
            "transform".into(),
            InputResource::Buffer {
                size: 16 * 4,
                stride: None,
                buffer: &self.transform_uniform,
            },
        );

        transient_res.insert(
            "vert_cfg".into(),
            InputResource::Buffer {
                size: 1 * 4,
                stride: None,
                buffer: &self.vert_config,
            },
        );

        self.render_graph.update_transient_cache(&transient_res);

        let valid = self
            .render_graph
            .validate(&transient_res, &rhai::Map::default())
            .unwrap();

        if !valid {
            log::error!("graph validation error");
        }

        self.render_graph
            .execute_with_encoder(
                &state,
                &transient_res,
                &rhai::Map::default(),
                encoder,
            )
            .unwrap();

        Ok(())
    }
}

pub struct PathRenderer {
    render_graph: Graph,

    path_index: Arc<PathIndex>,
    graph_curves: layout::GraphPathCurves,
    // layout: GfaLayout,
    camera: DynamicCamera2d,

    graph_scalars: rhai::Map,

    uniform_buf: wgpu::Buffer,

    annotations: AnnotationStore,
    annotation_cache: Vec<(Vec2, String)>,

    path_curve_buffers: PathCurveBuffers,
    draw_node: NodeId,
}

fn draw_annotations(
    cache: &[(Vec2, String)],
    painter: &egui::Painter,
    window_dims: Vec2,
    camera: &DynamicCamera2d,
) {
    for (pos, text) in cache.iter() {
        let norm_p = camera.transform_world_to_screen(*pos);
        let size = window_dims;
        let p = norm_p * size;

        let anchor = egui::Align2::CENTER_CENTER;
        let font = egui::FontId::proportional(16.0);
        painter.text(
            egui::pos2(p.x, p.y),
            anchor,
            text,
            font,
            egui::Color32::WHITE,
        );
    }
}

impl PathRenderer {
    pub fn init(
        event_loop: &EventLoopWindowTarget<()>,
        state: &State,
        window: &WindowState,
        path_index: Arc<PathIndex>,
        layout_tsv: impl AsRef<std::path::Path>,
    ) -> Result<Self> {
        let graph_curves = GraphPathCurves::from_path_index_and_layout_tsv(
            &path_index,
            layout_tsv,
        )?;

        let mut graph = Graph::new();

        let draw_schema = {
            let vert_src = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/shaders/lyon.vert.spv"
            ));
            let frag_src = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/shaders/flat.frag.spv"
            ));

            let primitive = wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Cw,
                cull_mode: None,
                // cull_mode: Some(wgpu::Face::Front),
                polygon_mode: wgpu::PolygonMode::Fill,

                strip_index_format: None,
                unclipped_depth: false,
                conservative: false,
            };

            graph.add_graphics_schema_custom(
                state,
                vert_src,
                frag_src,
                primitive,
                wgpu::VertexStepMode::Vertex,
                ["vertex_in"],
                Some("indices"),
                &[window.surface_format],
            )?
        };

        let camera = {
            let center = Vec2::zero();
            let size = Vec2::new(4.0, 3.0);
            let (min, max) = graph_curves.aabb;
            let mut camera = DynamicCamera2d::new(center, size);
            camera.fit_region_keep_aspect(min, max);
            camera
        };

        let egui =
            EguiCtx::init(state, window.surface_format, event_loop, None);

        let uniform_data = camera.to_matrix();

        let uniform_buf = state.device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("Uniform Buffer"),
                contents: bytemuck::cast_slice(&[uniform_data]),
                usage: wgpu::BufferUsages::UNIFORM
                    | wgpu::BufferUsages::COPY_DST,
            },
        );

        let draw_node = graph.add_node(draw_schema);

        graph.add_link_from_transient("vertices", draw_node, 0);
        graph.add_link_from_transient("indices", draw_node, 1);
        graph.add_link_from_transient("swapchain", draw_node, 2);

        // set 0, binding 0, transform matrix
        graph.add_link_from_transient("transform", draw_node, 3);

        let path_ids = 0..path_index.path_names.len();
        let path_curve_buffers =
            graph_curves.tessellate_paths(&state.device, path_ids)?;

        let annotations = AnnotationStore::default();

        Ok(Self {
            render_graph: graph,

            path_index,

            camera,
            graph_scalars: rhai::Map::default(),
            uniform_buf,
            annotations,
            annotation_cache: Vec::new(),
            path_curve_buffers,
            draw_node,

            graph_curves,
            // layout,
        })
    }
}

impl AppWindow for PathRenderer {
    fn update(
        &mut self,
        _handle: &tokio::runtime::Handle,
        _state: &raving_wgpu::State,
        window: &raving_wgpu::WindowState,
        egui_ctx: &mut EguiCtx,
        dt: f32,
    ) {
        /*
        dbg!();
        egui_ctx.run(&window.window, |ctx| {
            dbg!();
            let painter = ctx.debug_painter();

            let origin = Vec2::new(40000.0, 180000.0);
            let norm_p = self.camera.transform_world_to_screen(origin);

            let size = window.window.inner_size();
            let size = Vec2::new(size.width as f32, size.height as f32);
            let p = norm_p * size;

            let stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
            let p = egui::pos2(p.x, p.y);

            let window_dims = size;
            draw_annotations(
                &self.annotation_cache,
                &painter,
                window_dims,
                &self.camera,
            );
        });

        dbg!();

        let any_touches = egui_ctx.ctx().input().any_touches();

        if any_touches {
            self.camera.stop();
        }

        self.camera.update(dt);

        let (scroll, delta, primary_down) = {
            let input = &egui_ctx.ctx().input();
            let scroll = input.scroll_delta;
            let pointer = &input.pointer;
            let delta = pointer.delta();
            let primary_down = pointer.primary_down();

            (scroll, delta, primary_down)
        };

        let win_size = {
            let s = window.window.inner_size();
            ultraviolet::Vec2::new(s.width as f32, s.height as f32)
        };

        let pos = egui_ctx.pointer_interact_pos();

        if let Some(touch) = egui_ctx.ctx().multi_touch() {
            let t = touch.translation_delta;
            let z = 2.0 - touch.zoom_delta;
            let t = ultraviolet::Vec2::new(-t.x / win_size.x, t.y / win_size.y);

            self.camera.blink(t);
            self.camera.size *= z;
        } else if primary_down {
            let delta = ultraviolet::Vec2::new(
                -delta.x / win_size.x,
                delta.y / win_size.y,
            );
            self.camera.blink(delta);
        }
        dbg!();
        */
    }

    fn on_event(
        &mut self,
        window_dims: [u32; 2],
        event: &winit::event::WindowEvent,
    ) -> bool {
        // TODO do stuff; currently handled in update() via egui

        false
    }

    fn on_resize(
        &mut self,
        _state: &raving_wgpu::State,
        old_window_dims: [u32; 2],
        new_window_dims: [u32; 2],
    ) -> anyhow::Result<()> {
        let [ow, oh] = old_window_dims;
        let [nw, nh] = new_window_dims;

        let old = Vec2::new(ow as f32, oh as f32);
        let new = Vec2::new(nw as f32, nh as f32);

        let div = new / old;
        self.camera.resize_relative(div);

        Ok(())
    }

    fn render(
        &mut self,
        state: &raving_wgpu::State,
        window: &WindowState,
        // output: &wgpu::SurfaceTexture,
        // window_dims: PhysicalSize<u32>,
        swapchain_view: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
    ) -> anyhow::Result<()> {
        let size: [u32; 2] = window.window.inner_size().into();

        let mut transient_res: HashMap<String, InputResource<'_>> =
            HashMap::default();

        let buffers = &self.path_curve_buffers;

        {
            let uniform_data = self.camera.to_matrix();
            state.queue.write_buffer(
                &self.uniform_buf,
                0,
                bytemuck::cast_slice(&[uniform_data]),
            );
        }

        let format = window.surface_format;

        transient_res.insert(
            "swapchain".into(),
            InputResource::Texture {
                size,
                format,
                texture: None,
                view: Some(&swapchain_view),
                sampler: None,
            },
        );

        let stride = 8;
        let v_size = stride * buffers.total_vertices;
        let i_size = 4 * buffers.total_indices;

        transient_res.insert(
            "vertices".into(),
            InputResource::Buffer {
                size: v_size,
                stride: Some(stride),
                buffer: &buffers.vertex_buffer,
            },
        );

        transient_res.insert(
            "indices".into(),
            InputResource::Buffer {
                size: i_size,
                stride: Some(4),
                buffer: &buffers.index_buffer,
            },
        );

        transient_res.insert(
            "transform".into(),
            InputResource::Buffer {
                size: 16 * 4,
                stride: None,
                buffer: &self.uniform_buf,
            },
        );

        self.render_graph.update_transient_cache(&transient_res);

        // log::warn!("validating graph");
        let valid = self
            .render_graph
            .validate(&transient_res, &self.graph_scalars)
            .unwrap();

        if !valid {
            log::error!("graph validation error");
        }

        /*
        self.render_graph.execute_with_encoder(
            &state,
            &transient_res,
            &self.graph_scalars,
            encoder,
        )?;
        */

        Ok(())
    }
}

pub fn parse_args() -> std::result::Result<Args, pico_args::Error> {
    let mut pargs = pico_args::Arguments::from_env();

    let args = Args {
        gfa: pargs.free_from_os_str(parse_path)?,
        tsv: pargs.free_from_os_str(parse_path)?,
        annotations: pargs.opt_value_from_os_str("--bed", parse_path)?,
    };

    Ok(args)
}

fn parse_path(s: &std::ffi::OsStr) -> Result<std::path::PathBuf, &'static str> {
    Ok(s.into())
}
