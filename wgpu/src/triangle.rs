//! Draw meshes of triangles.
mod msaa;

use crate::core::Transformation;
use crate::graphics::antialiasing::{self, Antialiasing};
use crate::graphics::Target;
use crate::layer::mesh::{self, Mesh};
use crate::Buffer;

const INITIAL_INDEX_COUNT: usize = 1_000;
const INITIAL_VERTEX_COUNT: usize = 1_000;

#[derive(Debug)]
pub struct Pipeline {
    format: wgpu::TextureFormat,
    format_features: wgpu::TextureFormatFeatures,
    layers: Vec<Layer>,
    prepare_layer: usize,
    state: State,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum State {
    Unused,
    Ready {
        blit: Option<msaa::Blit>,
        solid: solid::Pipeline,
        gradient: gradient::Pipeline,
    },
}

impl Pipeline {
    pub fn new(
        adapter: &wgpu::Adapter,
        format: wgpu::TextureFormat,
    ) -> Pipeline {
        let format_features = adapter.get_texture_format_features(format);

        Pipeline {
            format,
            format_features,
            layers: Vec::new(),
            prepare_layer: 0,
            state: State::Unused,
        }
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        antialiasing: Antialiasing,
        target: &Target,
        meshes: &[Mesh<'_>],
    ) {
        #[cfg(feature = "tracing")]
        let _ = tracing::info_span!("Wgpu::Triangle", "PREPARE").entered();

        if self.prepare_layer == 0 {
            self.compile(device, antialiasing, target);
        }

        let State::Ready {
            solid, gradient, ..
        } = &mut self.state
        else {
            return;
        };

        let transformation = target.viewport.scaled_projection();

        if self.layers.len() <= self.prepare_layer {
            self.layers.push(Layer::new(device, solid, gradient));
        }

        let layer = &mut self.layers[self.prepare_layer];
        layer.prepare(
            device,
            encoder,
            belt,
            solid,
            gradient,
            meshes,
            transformation,
        );

        self.prepare_layer += 1;
    }

    pub fn render(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        frame: &wgpu::TextureView,
        target: &Target,
        layer: usize,
        meshes: &[Mesh<'_>],
    ) {
        #[cfg(feature = "tracing")]
        let _ = tracing::info_span!("Wgpu::Triangle", "DRAW").entered();

        let State::Ready {
            blit,
            solid,
            gradient,
        } = &mut self.state
        else {
            return;
        };

        let target_size = target.viewport.physical_size();

        {
            let (attachment, resolve_target, load) = if let Some(blit) = blit {
                let (attachment, resolve_target) =
                    blit.targets(device, target_size.width, target_size.height);

                (
                    attachment,
                    Some(resolve_target),
                    wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                )
            } else {
                (frame, None, wgpu::LoadOp::Load)
            };

            let mut render_pass =
                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("iced_wgpu.triangle.render_pass"),
                    color_attachments: &[Some(
                        wgpu::RenderPassColorAttachment {
                            view: attachment,
                            resolve_target,
                            ops: wgpu::Operations {
                                load,
                                store: wgpu::StoreOp::Store,
                            },
                        },
                    )],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

            let layer = &mut self.layers[layer];

            layer.render(
                solid,
                gradient,
                meshes,
                target.viewport.scale_factor() as f32,
                &mut render_pass,
            );
        }

        if let Some(blit) = blit {
            blit.draw(encoder, frame);
        }
    }

    pub fn end_frame(&mut self) {
        self.prepare_layer = 0;
    }

    fn compile(
        &mut self,
        device: &wgpu::Device,
        antialiasing: Antialiasing,
        target: &Target,
    ) {
        let msaa = match antialiasing {
            Antialiasing::Disabled => None,
            Antialiasing::MSAA(msaa) => Some(msaa),
            Antialiasing::Automatic => {
                if target.scale_factor > 2.0 {
                    None
                } else if cfg!(target_os = "macos")
                    && target.scale_factor >= 1.5
                {
                    Some(antialiasing::MSAA::X2)
                } else {
                    Some(antialiasing::MSAA::X4)
                }
            }
        }
        .map(|msaa| {
            if self
                .format_features
                .flags
                .sample_count_supported(msaa.sample_count())
            {
                msaa
            } else {
                antialiasing::MSAA::X4
            }
        });

        match self.state {
            State::Ready { ref blit, .. }
                if blit.as_ref().map(msaa::Blit::strategy) == msaa => {}
            _ => {
                self.state = State::Ready {
                    blit: msaa
                        .map(|msaa| msaa::Blit::new(device, self.format, msaa)),
                    solid: solid::Pipeline::new(device, self.format, msaa),
                    gradient: gradient::Pipeline::new(
                        device,
                        self.format,
                        msaa,
                    ),
                };

                self.layers.clear();
                self.prepare_layer = 0;
            }
        }
    }
}

#[derive(Debug)]
struct Layer {
    index_buffer: Buffer<u32>,
    index_strides: Vec<u32>,
    solid: solid::Layer,
    gradient: gradient::Layer,
}

impl Layer {
    fn new(
        device: &wgpu::Device,
        solid: &solid::Pipeline,
        gradient: &gradient::Pipeline,
    ) -> Self {
        Self {
            index_buffer: Buffer::new(
                device,
                "iced_wgpu.triangle.index_buffer",
                INITIAL_INDEX_COUNT,
                wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            ),
            index_strides: Vec::new(),
            solid: solid::Layer::new(device, &solid.constants_layout),
            gradient: gradient::Layer::new(device, &gradient.constants_layout),
        }
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        solid: &solid::Pipeline,
        gradient: &gradient::Pipeline,
        meshes: &[Mesh<'_>],
        transformation: Transformation,
    ) {
        // Count the total amount of vertices & indices we need to handle
        let count = mesh::attribute_count_of(meshes);

        // Then we ensure the current attribute buffers are big enough, resizing if necessary.
        // We are not currently using the return value of these functions as we have no system in
        // place to calculate mesh diff, or to know whether or not that would be more performant for
        // the majority of use cases. Therefore we will write GPU data every frame (for now).
        let _ = self.index_buffer.resize(device, count.indices);
        let _ = self.solid.vertices.resize(device, count.solid_vertices);
        let _ = self
            .gradient
            .vertices
            .resize(device, count.gradient_vertices);

        if self.solid.uniforms.resize(device, count.solids) {
            self.solid.constants = solid::Layer::bind_group(
                device,
                &self.solid.uniforms.raw,
                &solid.constants_layout,
            );
        }

        if self.gradient.uniforms.resize(device, count.gradients) {
            self.gradient.constants = gradient::Layer::bind_group(
                device,
                &self.gradient.uniforms.raw,
                &gradient.constants_layout,
            );
        }

        self.index_strides.clear();
        self.index_buffer.clear();
        self.solid.vertices.clear();
        self.solid.uniforms.clear();
        self.gradient.vertices.clear();
        self.gradient.uniforms.clear();

        let mut solid_vertex_offset = 0;
        let mut solid_uniform_offset = 0;
        let mut gradient_vertex_offset = 0;
        let mut gradient_uniform_offset = 0;
        let mut index_offset = 0;

        for mesh in meshes {
            let indices = mesh.indices();

            let uniforms =
                Uniforms::new(transformation * mesh.transformation());

            index_offset += self.index_buffer.write(
                device,
                encoder,
                belt,
                index_offset,
                indices,
            );

            self.index_strides.push(indices.len() as u32);

            match mesh {
                Mesh::Solid { buffers, .. } => {
                    solid_vertex_offset += self.solid.vertices.write(
                        device,
                        encoder,
                        belt,
                        solid_vertex_offset,
                        &buffers.vertices,
                    );

                    solid_uniform_offset += self.solid.uniforms.write(
                        device,
                        encoder,
                        belt,
                        solid_uniform_offset,
                        &[uniforms],
                    );
                }
                Mesh::Gradient { buffers, .. } => {
                    gradient_vertex_offset += self.gradient.vertices.write(
                        device,
                        encoder,
                        belt,
                        gradient_vertex_offset,
                        &buffers.vertices,
                    );

                    gradient_uniform_offset += self.gradient.uniforms.write(
                        device,
                        encoder,
                        belt,
                        gradient_uniform_offset,
                        &[uniforms],
                    );
                }
            }
        }
    }

    fn render<'a>(
        &'a self,
        solid: &'a solid::Pipeline,
        gradient: &'a gradient::Pipeline,
        meshes: &[Mesh<'_>],
        scale_factor: f32,
        render_pass: &mut wgpu::RenderPass<'a>,
    ) {
        let mut num_solids = 0;
        let mut num_gradients = 0;
        let mut last_is_solid = None;

        for (index, mesh) in meshes.iter().enumerate() {
            let clip_bounds = (mesh.clip_bounds() * scale_factor).snap();

            if clip_bounds.width < 1 || clip_bounds.height < 1 {
                continue;
            }

            render_pass.set_scissor_rect(
                clip_bounds.x,
                clip_bounds.y,
                clip_bounds.width,
                clip_bounds.height,
            );

            match mesh {
                Mesh::Solid { .. } => {
                    if !last_is_solid.unwrap_or(false) {
                        render_pass.set_pipeline(&solid.pipeline);

                        last_is_solid = Some(true);
                    }

                    render_pass.set_bind_group(
                        0,
                        &self.solid.constants,
                        &[(num_solids * std::mem::size_of::<Uniforms>())
                            as u32],
                    );

                    render_pass.set_vertex_buffer(
                        0,
                        self.solid.vertices.slice_from_index(num_solids),
                    );

                    num_solids += 1;
                }
                Mesh::Gradient { .. } => {
                    if last_is_solid.unwrap_or(true) {
                        render_pass.set_pipeline(&gradient.pipeline);

                        last_is_solid = Some(false);
                    }

                    render_pass.set_bind_group(
                        0,
                        &self.gradient.constants,
                        &[(num_gradients * std::mem::size_of::<Uniforms>())
                            as u32],
                    );

                    render_pass.set_vertex_buffer(
                        0,
                        self.gradient.vertices.slice_from_index(num_gradients),
                    );

                    num_gradients += 1;
                }
            };

            render_pass.set_index_buffer(
                self.index_buffer.slice_from_index(index),
                wgpu::IndexFormat::Uint32,
            );

            render_pass.draw_indexed(0..self.index_strides[index], 0, 0..1);
        }
    }
}

fn fragment_target(
    texture_format: wgpu::TextureFormat,
) -> wgpu::ColorTargetState {
    wgpu::ColorTargetState {
        format: texture_format,
        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
        write_mask: wgpu::ColorWrites::ALL,
    }
}

fn primitive_state() -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        front_face: wgpu::FrontFace::Cw,
        ..Default::default()
    }
}

fn multisample_state(
    msaa: Option<antialiasing::MSAA>,
) -> wgpu::MultisampleState {
    wgpu::MultisampleState {
        count: msaa.map(antialiasing::MSAA::sample_count).unwrap_or(1),
        mask: !0,
        alpha_to_coverage_enabled: false,
    }
}

#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Uniforms {
    transform: [f32; 16],
    /// Uniform values must be 256-aligned;
    /// see: [`wgpu::Limits`] `min_uniform_buffer_offset_alignment`.
    _padding: [f32; 48],
}

impl Uniforms {
    pub fn new(transform: Transformation) -> Self {
        Self {
            transform: transform.into(),
            _padding: [0.0; 48],
        }
    }

    pub fn entry() -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(
                    std::mem::size_of::<Self>() as u64,
                ),
            },
            count: None,
        }
    }

    pub fn min_size() -> Option<wgpu::BufferSize> {
        wgpu::BufferSize::new(std::mem::size_of::<Self>() as u64)
    }
}

mod solid {
    use crate::graphics::antialiasing;
    use crate::graphics::mesh;
    use crate::triangle;
    use crate::Buffer;

    #[derive(Debug)]
    pub struct Pipeline {
        pub pipeline: wgpu::RenderPipeline,
        pub constants_layout: wgpu::BindGroupLayout,
    }

    #[derive(Debug)]
    pub struct Layer {
        pub vertices: Buffer<mesh::SolidVertex2D>,
        pub uniforms: Buffer<triangle::Uniforms>,
        pub constants: wgpu::BindGroup,
    }

    impl Layer {
        pub fn new(
            device: &wgpu::Device,
            constants_layout: &wgpu::BindGroupLayout,
        ) -> Self {
            let vertices = Buffer::new(
                device,
                "iced_wgpu.triangle.solid.vertex_buffer",
                triangle::INITIAL_VERTEX_COUNT,
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            );

            let uniforms = Buffer::new(
                device,
                "iced_wgpu.triangle.solid.uniforms",
                1,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            );

            let constants =
                Self::bind_group(device, &uniforms.raw, constants_layout);

            Self {
                vertices,
                uniforms,
                constants,
            }
        }

        pub fn bind_group(
            device: &wgpu::Device,
            buffer: &wgpu::Buffer,
            layout: &wgpu::BindGroupLayout,
        ) -> wgpu::BindGroup {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("iced_wgpu.triangle.solid.bind_group"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(
                        wgpu::BufferBinding {
                            buffer,
                            offset: 0,
                            size: triangle::Uniforms::min_size(),
                        },
                    ),
                }],
            })
        }
    }

    impl Pipeline {
        pub fn new(
            device: &wgpu::Device,
            format: wgpu::TextureFormat,
            msaa: Option<antialiasing::MSAA>,
        ) -> Self {
            let constants_layout = device.create_bind_group_layout(
                &wgpu::BindGroupLayoutDescriptor {
                    label: Some("iced_wgpu.triangle.solid.bind_group_layout"),
                    entries: &[triangle::Uniforms::entry()],
                },
            );

            let layout = device.create_pipeline_layout(
                &wgpu::PipelineLayoutDescriptor {
                    label: Some("iced_wgpu.triangle.solid.pipeline_layout"),
                    bind_group_layouts: &[&constants_layout],
                    push_constant_ranges: &[],
                },
            );

            let shader =
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("iced_wgpu.triangle.solid.shader"),
                    source: wgpu::ShaderSource::Wgsl(
                        std::borrow::Cow::Borrowed(concat!(
                            include_str!("shader/triangle.wgsl"),
                            "\n",
                            include_str!("shader/triangle/solid.wgsl"),
                        )),
                    ),
                });

            let pipeline =
                device.create_render_pipeline(
                    &wgpu::RenderPipelineDescriptor {
                        label: Some("iced_wgpu::triangle::solid pipeline"),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &shader,
                            entry_point: "solid_vs_main",
                            buffers: &[wgpu::VertexBufferLayout {
                                array_stride: std::mem::size_of::<
                                    mesh::SolidVertex2D,
                                >(
                                )
                                    as u64,
                                step_mode: wgpu::VertexStepMode::Vertex,
                                attributes: &wgpu::vertex_attr_array!(
                                    // Position
                                    0 => Float32x2,
                                    // Color
                                    1 => Float32x4,
                                ),
                            }],
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &shader,
                            entry_point: "solid_fs_main",
                            targets: &[Some(triangle::fragment_target(format))],
                        }),
                        primitive: triangle::primitive_state(),
                        depth_stencil: None,
                        multisample: triangle::multisample_state(msaa),
                        multiview: None,
                    },
                );

            Self {
                pipeline,
                constants_layout,
            }
        }
    }
}

mod gradient {
    use crate::graphics::antialiasing;
    use crate::graphics::color;
    use crate::graphics::mesh;
    use crate::triangle;
    use crate::Buffer;

    #[derive(Debug)]
    pub struct Pipeline {
        pub pipeline: wgpu::RenderPipeline,
        pub constants_layout: wgpu::BindGroupLayout,
    }

    #[derive(Debug)]
    pub struct Layer {
        pub vertices: Buffer<mesh::GradientVertex2D>,
        pub uniforms: Buffer<triangle::Uniforms>,
        pub constants: wgpu::BindGroup,
    }

    impl Layer {
        pub fn new(
            device: &wgpu::Device,
            constants_layout: &wgpu::BindGroupLayout,
        ) -> Self {
            let vertices = Buffer::new(
                device,
                "iced_wgpu.triangle.gradient.vertex_buffer",
                triangle::INITIAL_VERTEX_COUNT,
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            );

            let uniforms = Buffer::new(
                device,
                "iced_wgpu.triangle.gradient.uniforms",
                1,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            );

            let constants =
                Self::bind_group(device, &uniforms.raw, constants_layout);

            Self {
                vertices,
                uniforms,
                constants,
            }
        }

        pub fn bind_group(
            device: &wgpu::Device,
            uniform_buffer: &wgpu::Buffer,
            layout: &wgpu::BindGroupLayout,
        ) -> wgpu::BindGroup {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("iced_wgpu.triangle.gradient.bind_group"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(
                        wgpu::BufferBinding {
                            buffer: uniform_buffer,
                            offset: 0,
                            size: triangle::Uniforms::min_size(),
                        },
                    ),
                }],
            })
        }
    }

    impl Pipeline {
        pub fn new(
            device: &wgpu::Device,
            format: wgpu::TextureFormat,
            antialiasing: Option<antialiasing::MSAA>,
        ) -> Self {
            let constants_layout = device.create_bind_group_layout(
                &wgpu::BindGroupLayoutDescriptor {
                    label: Some(
                        "iced_wgpu.triangle.gradient.bind_group_layout",
                    ),
                    entries: &[triangle::Uniforms::entry()],
                },
            );

            let layout = device.create_pipeline_layout(
                &wgpu::PipelineLayoutDescriptor {
                    label: Some("iced_wgpu.triangle.gradient.pipeline_layout"),
                    bind_group_layouts: &[&constants_layout],
                    push_constant_ranges: &[],
                },
            );

            let shader =
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("iced_wgpu.triangle.gradient.shader"),
                    source: wgpu::ShaderSource::Wgsl(
                        std::borrow::Cow::Borrowed(
                            if color::GAMMA_CORRECTION {
                                concat!(
                                    include_str!("shader/triangle.wgsl"),
                                    "\n",
                                    include_str!(
                                        "shader/triangle/gradient.wgsl"
                                    ),
                                    "\n",
                                    include_str!("shader/color/oklab.wgsl")
                                )
                            } else {
                                concat!(
                                    include_str!("shader/triangle.wgsl"),
                                    "\n",
                                    include_str!(
                                        "shader/triangle/gradient.wgsl"
                                    ),
                                    "\n",
                                    include_str!(
                                        "shader/color/linear_rgb.wgsl"
                                    )
                                )
                            },
                        ),
                    ),
                });

            let pipeline = device.create_render_pipeline(
                &wgpu::RenderPipelineDescriptor {
                    label: Some("iced_wgpu.triangle.gradient.pipeline"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: "gradient_vs_main",
                        buffers: &[wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<
                                mesh::GradientVertex2D,
                            >()
                                as u64,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &wgpu::vertex_attr_array!(
                                // Position
                                0 => Float32x2,
                                // Colors 1-2
                                1 => Uint32x4,
                                // Colors 3-4
                                2 => Uint32x4,
                                // Colors 5-6
                                3 => Uint32x4,
                                // Colors 7-8
                                4 => Uint32x4,
                                // Offsets
                                5 => Uint32x4,
                                // Direction
                                6 => Float32x4
                            ),
                        }],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: "gradient_fs_main",
                        targets: &[Some(triangle::fragment_target(format))],
                    }),
                    primitive: triangle::primitive_state(),
                    depth_stencil: None,
                    multisample: triangle::multisample_state(antialiasing),
                    multiview: None,
                },
            );

            Self {
                pipeline,
                constants_layout,
            }
        }
    }
}
