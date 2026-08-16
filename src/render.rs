//! Drawing a frame onto a surface, corner-pinned and colour-converted.
//!
//! This is a renderer, not a window. It draws into a [`wgpu::TextureView`] —
//! one belonging to a window vtome opened, to a surface a Tauri application
//! handed over, or to an offscreen texture in a test. The `window` feature adds
//! the first of those; `render` on its own is enough for the other two.
//!
//! # What the shader does
//!
//! Everything, deliberately. The vertex stage draws one triangle covering the
//! whole target and carries no attributes at all; the fragment stage takes its
//! own pixel coordinate, pushes it back through the quad's **inverse**
//! homography, and looks up the texel that belongs there.
//!
//! Working backwards like this is what makes an arbitrary convex quad
//! perspective-correct for free: the divide by `w` happens per pixel, which is
//! exactly what the two-triangle approach cannot do. There is no seam along the
//! diagonal because there is no diagonal.
//!
//! Colour conversion rides along in the same pass — the YUV planes are sampled
//! as separate textures and multiplied by the matrix from
//! [`ColorSpace::yuv_to_rgb`](crate::color::ColorSpace::yuv_to_rgb), so nothing
//! is converted on the CPU and nothing is copied twice.
//!
//! ```no_run
//! use vtome::geometry::{Quad, Rect};
//! use vtome::render::{Gpu, Renderer};
//!
//! let gpu = Gpu::new()?;
//! let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm)?;
//!
//! let frame = vtome::load_image("poster.png")?;
//! let quad = Quad::keystone(Rect::from_size(1920.0, 1080.0), 200.0);
//!
//! // Straight to pixels, with no window involved.
//! let pixels = renderer.render_to_rgba(&gpu, &frame, 1920, 1080, quad, 1.0)?;
//! # Ok::<(), vtome::Error>(())
//! ```

use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat};
use crate::geometry::{Mat3, Quad};

/// The shader. One file, because splitting a 60-line shader across files helps
/// nobody.
const SHADER: &str = include_str!("render/present.wgsl");

/// Buffer-to-texture copies want rows on this boundary. Texture writes do not,
/// but the readback path does, and padding once is simpler than two rules.
const ROW_ALIGNMENT: u32 = 256;

/// A GPU, opened.
///
/// Holding the adapter as well as the device is deliberate: which adapter was
/// chosen is the first question when something renders slowly, and
/// [`Gpu::describe`] answers it without the caller keeping its own copy.
pub struct Gpu {
    /// The wgpu instance, kept alive for as long as anything it made.
    pub instance: wgpu::Instance,
    /// The adapter that was chosen.
    pub adapter: wgpu::Adapter,
    /// The open device.
    pub device: wgpu::Device,
    /// Its queue.
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Opens a GPU with no surface attached.
    ///
    /// Enough for offscreen rendering and for tests. A window's surface can be
    /// attached afterwards — the adapter is chosen without one, which is fine
    /// everywhere except WebGL.
    ///
    /// # Errors
    ///
    /// [`Error::Render`] if there is no adapter, or the device will not open.
    pub fn new() -> Result<Self> {
        // No display handle: nothing here will present to a window. The
        // `window` feature builds its instance the other way, because a surface
        // has to be made from an instance that knows about the display.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

        Self::from_instance(instance, None)
    }

    /// Opens a GPU on an instance the caller already made, optionally choosing
    /// an adapter that works with `surface`.
    ///
    /// This is the embedding path: a host application — Tauri, a game engine —
    /// owns the window and the instance, and vtome draws into it.
    ///
    /// # Errors
    ///
    /// As [`Gpu::new`].
    pub fn from_instance(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: surface,
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|error| Error::Render {
            reason: format!("no usable graphics adapter: {error}"),
        })?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vtome"),
            ..Default::default()
        }))
        .map_err(|error| Error::Render {
            reason: format!("the graphics device would not open: {error}"),
        })?;

        Ok(Gpu {
            instance,
            adapter,
            device,
            queue,
        })
    }

    /// The adapter, in words: its name, backend, and kind.
    pub fn describe(&self) -> String {
        let info = self.adapter.get_info();

        format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type)
    }

    /// The largest square texture this device will hold, which is the ceiling
    /// on the picture size it can draw.
    pub fn max_texture_size(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
}

/// How the planes are arranged, as the shader sees it.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// One RGBA texture; no conversion.
    Rgba = 0,
    /// Three single-channel planes: Y, U, V.
    Planar = 1,
    /// Two planes: Y, then interleaved UV.
    BiPlanar = 2,
}

/// The uniform block, laid out to WGSL's rules.
///
/// Every field is a multiple of 16 bytes or padded into one, because a
/// mismatched layout does not fail — it renders something subtly, unfixably
/// wrong.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    /// Rows of the inverse homography: surface pixels back to the unit square.
    inverse: [[f32; 4]; 3],
    /// Rows of the YUV→RGB matrix, with the offset in `w`.
    color: [[f32; 4]; 3],
    /// Target size in pixels. `target` on its own is a reserved word in WGSL,
    /// which the shader will not compile with and does not explain gently.
    target_size: [f32; 2],
    /// 0.0 to 1.0.
    opacity: f32,
    /// Which [`Mode`].
    mode: u32,
}

/// The textures one frame lives in, and what shape they were made for.
struct Planes {
    textures: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    format: PixelFormat,
    /// The colour space of the last frame written into them.
    ///
    /// Kept here so drawing needs no frame in hand: a player redraws the same
    /// picture whenever the window is resized or another layer moves.
    color: crate::color::ColorSpace,
    /// Its bit depth, for the same reason.
    bit_depth: u32,
}

/// Draws frames.
///
/// One renderer serves one target format. It holds the pipeline, the sampler,
/// and the textures for the last frame uploaded — so playing a film reuses the
/// same textures for every frame and allocates nothing after the first.
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    planes: Option<Planes>,
    format: wgpu::TextureFormat,
}

impl Renderer {
    /// A renderer drawing into textures of `format`.
    ///
    /// # Errors
    ///
    /// [`Error::Render`] if the pipeline will not build.
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> Result<Self> {
        let device = &gpu.device;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vtome present"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        // Three plane slots always, whatever the format needs: a layout that
        // changed with the pixel format would mean rebuilding the pipeline
        // every time a playlist moved from an RGBA still to a YUV film.
        let mut entries = vec![wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }];

        for binding in 1..=3 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
        }

        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 4,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vtome planes"),
            entries: &entries,
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vtome present"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vtome present"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // Straight alpha: the shader hands back zero alpha outside
                    // the quad, which is how a trapezoid leaves the rest of the
                    // surface alone rather than painting it black.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vtome sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vtome uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Renderer {
            pipeline,
            layout,
            sampler,
            uniforms,
            planes: None,
            format,
        })
    }

    /// The target format this renderer draws into.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// Puts a frame's pixels on the GPU.
    ///
    /// Textures are reused when the next frame is the same size and format,
    /// which during playback is every frame after the first.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a pixel format with no shader path yet,
    /// [`Error::Render`] if the picture is larger than the device allows.
    pub fn upload(&mut self, gpu: &Gpu, frame: &Frame) -> Result<()> {
        // Checked before anything is allocated: a format with no shader path
        // should fail on the way in, not once the textures exist.
        mode_for(frame.format())?;

        let limit = gpu.max_texture_size();
        if frame.width() > limit || frame.height() > limit {
            return Err(Error::Render {
                reason: format!(
                    "{}×{} is past this device's {limit}-pixel texture limit",
                    frame.width(),
                    frame.height()
                ),
            });
        }

        let stale = self.planes.as_ref().is_none_or(|planes| {
            planes.width != frame.width()
                || planes.height != frame.height()
                || planes.format != frame.format()
        });

        if stale {
            self.planes = Some(self.allocate(gpu, frame)?);
        }

        let planes = self.planes.as_mut().expect("just allocated");
        planes.color = frame.color();
        planes.bit_depth = frame.format().bit_depth();

        let planes = self.planes.as_ref().expect("just allocated");

        for (index, texture) in planes.textures.iter().enumerate() {
            let (plane_width, plane_height) = frame
                .format()
                .plane_dimensions(index, frame.width(), frame.height())
                .expect("the texture count came from the plane count");

            let descriptor = frame.planes()[index];
            let row_bytes = plane_width as usize
                * frame.format().samples_per_pixel(index)
                * frame.format().bytes_per_sample();

            let data = frame.plane_data(index).ok_or_else(|| Error::Render {
                reason: format!("plane {index} is not where the frame says it is"),
            })?;

            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    // The decoder's own stride, padding and all — the texture
                    // takes it directly rather than the frame being repacked.
                    bytes_per_row: Some(descriptor.stride.max(row_bytes) as u32),
                    rows_per_image: Some(plane_height),
                },
                wgpu::Extent3d {
                    width: plane_width,
                    height: plane_height,
                    depth_or_array_layers: 1,
                },
            );
        }

        Ok(())
    }

    /// Makes the textures and the bind group for a frame's shape.
    fn allocate(&self, gpu: &Gpu, frame: &Frame) -> Result<Planes> {
        let format = frame.format();
        let mut textures = Vec::new();
        let mut views = Vec::new();

        for index in 0..format.plane_count() {
            let (width, height) = format
                .plane_dimensions(index, frame.width(), frame.height())
                .expect("index is below the plane count");

            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vtome plane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: texture_format(format, index),
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            views.push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
            textures.push(texture);
        }

        // The layout always has three plane slots; formats with fewer bind
        // their first plane again rather than the shader branching on a
        // binding that is not there.
        let filler = &views[0];
        let bound: Vec<&wgpu::TextureView> = (0..3)
            .map(|index| views.get(index).unwrap_or(filler))
            .collect();

        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vtome planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(bound[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(bound[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(bound[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        Ok(Planes {
            textures,
            bind_group,
            width: frame.width(),
            height: frame.height(),
            format,
            color: frame.color(),
            bit_depth: format.bit_depth(),
        })
    }

    /// Draws the frame most recently uploaded into `view`.
    ///
    /// `quad` is in the target's own pixels — see
    /// [`ResolvedPlacement::quad_in_window`](crate::ResolvedPlacement::quad_in_window),
    /// which is exactly that.
    ///
    /// # Errors
    ///
    /// [`Error::Placement`] if the quad is not convex, [`Error::Render`] if no
    /// frame has been uploaded.
    pub fn draw(
        &self,
        gpu: &Gpu,
        view: &wgpu::TextureView,
        target_width: u32,
        target_height: u32,
        quad: Quad,
        opacity: f32,
    ) -> Result<()> {
        let Some(planes) = self.planes.as_ref() else {
            return Err(Error::Render {
                reason: "nothing has been uploaded to draw".to_string(),
            });
        };

        // The inverse map: from a pixel on the target back to a texel. Working
        // this way round is what makes the perspective divide per-pixel.
        let inverse = quad.inverse_homography()?;

        let uniforms = Uniforms {
            inverse: rows_padded(inverse),
            color: planes.color.yuv_to_rgb(planes.bit_depth),
            target_size: [target_width as f32, target_height as f32],
            opacity: opacity.clamp(0.0, 1.0),
            mode: mode_for(planes.format)? as u32,
        };

        gpu.queue
            .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&uniforms));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vtome present"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vtome present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Transparent rather than black: what is outside the
                        // quad belongs to whatever is behind it.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &planes.bind_group, &[]);
            // Three vertices, no buffers: the vertex shader makes a triangle
            // big enough to cover the target out of its own index.
            pass.draw(0..3, 0..1);
        }

        gpu.queue.submit(std::iter::once(encoder.finish()));

        Ok(())
    }

    /// Draws a frame into an offscreen texture and reads it back as RGBA8.
    ///
    /// Slow by construction — it waits for the GPU and copies back over the
    /// bus — and exactly what a test, a thumbnail, or a still export wants.
    ///
    /// # Errors
    ///
    /// As [`upload`](Renderer::upload) and [`draw`](Renderer::draw), plus
    /// [`Error::Render`] if the readback fails.
    pub fn render_to_rgba(
        &mut self,
        gpu: &Gpu,
        frame: &Frame,
        width: u32,
        height: u32,
        quad: Quad,
        opacity: f32,
    ) -> Result<Vec<u8>> {
        if self.format != wgpu::TextureFormat::Rgba8Unorm {
            return Err(Error::Render {
                reason: format!(
                    "this renderer draws {:?}; reading back as RGBA8 needs one built for \
                     TextureFormat::Rgba8Unorm",
                    self.format
                ),
            });
        }

        self.upload(gpu, frame)?;

        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vtome offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        self.draw(gpu, &view, width, height, quad, opacity)?;

        // Buffer-to-texture copies want rows aligned; the padding comes back
        // off below.
        let unpadded = width * 4;
        let padded = unpadded.div_ceil(ROW_ALIGNMENT) * ROW_ALIGNMENT;

        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vtome readback"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vtome readback"),
            });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        gpu.queue.submit(std::iter::once(encoder.finish()));

        let (sender, receiver) = std::sync::mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });

        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| Error::Render {
                reason: format!("waiting for the GPU: {error}"),
            })?;

        receiver
            .recv()
            .map_err(|_| Error::Render {
                reason: "the readback never completed".to_string(),
            })?
            .map_err(|error| Error::Render {
                reason: format!("mapping the readback buffer: {error}"),
            })?;

        let mapped = readback
            .slice(..)
            .get_mapped_range()
            .map_err(|error| Error::Render {
                reason: format!("reading the mapped buffer: {error}"),
            })?;

        let mut pixels = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }

        drop(mapped);
        readback.unmap();

        Ok(pixels)
    }
}

/// Which shader path a pixel format takes.
fn mode_for(format: PixelFormat) -> Result<Mode> {
    Ok(match format {
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => Mode::Rgba,
        PixelFormat::I420 | PixelFormat::I422 | PixelFormat::I444 => Mode::Planar,
        PixelFormat::Nv12 => Mode::BiPlanar,
        PixelFormat::P010 => {
            return Err(Error::unsupported(
                "10-bit P010 needs 16-bit textures and a shifted sample; \
                 see planning/TODO.md §11",
            ))
        }
    })
}

/// The texture format one plane wants.
fn texture_format(format: PixelFormat, plane: usize) -> wgpu::TextureFormat {
    match (format, plane) {
        // Unorm rather than Srgb: the sRGB curve is a colour-management
        // decision and doing it silently here would double-apply it for video,
        // which carries its own transfer function.
        (PixelFormat::Rgba8, _) => wgpu::TextureFormat::Rgba8Unorm,
        (PixelFormat::Bgra8, _) => wgpu::TextureFormat::Bgra8Unorm,
        (PixelFormat::Nv12, 1) => wgpu::TextureFormat::Rg8Unorm,
        (PixelFormat::P010, 0) => wgpu::TextureFormat::R16Unorm,
        (PixelFormat::P010, _) => wgpu::TextureFormat::Rg16Unorm,
        _ => wgpu::TextureFormat::R8Unorm,
    }
}

/// The rows of a matrix as four-float vectors, which is what a uniform holds.
fn rows_padded(matrix: Mat3) -> [[f32; 4]; 3] {
    let Mat3(m) = matrix;

    std::array::from_fn(|row| [m[row][0] as f32, m[row][1] as f32, m[row][2] as f32, 0.0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorSpace;
    use crate::geometry::{Point, Rect};
    use std::time::Duration;

    /// Every test here needs a GPU. There is not always one — a container, a CI
    /// runner without a display — and a skipped test says so rather than
    /// failing for a reason that is nothing to do with the code.
    fn gpu() -> Option<Gpu> {
        match Gpu::new() {
            Ok(gpu) => Some(gpu),
            Err(error) => {
                eprintln!("skipping: no GPU here ({error})");
                None
            }
        }
    }

    /// A 2×2 picture, one colour per quadrant, so orientation is visible in the
    /// output rather than inferred.
    fn quadrants() -> Frame {
        let pixels = vec![
            255, 0, 0, 255, // top-left: red
            0, 255, 0, 255, // top-right: green
            0, 0, 255, 255, // bottom-left: blue
            255, 255, 0, 255, // bottom-right: yellow
        ];

        Frame::packed(
            2,
            2,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::ZERO,
            pixels,
        )
        .unwrap()
    }

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let start = ((y * width + x) * 4) as usize;
        pixels[start..start + 4].try_into().unwrap()
    }

    #[test]
    fn a_frame_drawn_over_the_whole_target_keeps_its_orientation() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let size = 64;
        let quad = Quad::from_rect(Rect::from_size(size as f64, size as f64));
        let pixels = renderer
            .render_to_rgba(&gpu, &quadrants(), size, size, quad, 1.0)
            .unwrap();

        // Corners, well inside the clamped edge texels.
        assert_eq!(pixel(&pixels, size, 4, 4), [255, 0, 0, 255], "top-left");
        assert_eq!(pixel(&pixels, size, 59, 4), [0, 255, 0, 255], "top-right");
        assert_eq!(pixel(&pixels, size, 4, 59), [0, 0, 255, 255], "bottom-left");
        assert_eq!(
            pixel(&pixels, size, 59, 59),
            [255, 255, 0, 255],
            "bottom-right"
        );
    }

    /// The quad is a window onto the surface, not a scaling of it: everything
    /// outside stays untouched so a trapezoid does not paint a black box.
    #[test]
    fn outside_the_quad_is_left_transparent() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let size = 64;
        let quad = Quad::from_rect(Rect::new(0.0, 0.0, 32.0, 64.0));
        let pixels = renderer
            .render_to_rgba(&gpu, &quadrants(), size, size, quad, 1.0)
            .unwrap();

        assert_eq!(pixel(&pixels, size, 8, 32)[3], 255, "inside the quad");
        assert_eq!(pixel(&pixels, size, 48, 32)[3], 0, "outside the quad");
    }

    /// The trapezoid, end to end: a keystoned quad has to be narrower at the
    /// top than at the bottom, and the shader is the only thing that decides
    /// that.
    #[test]
    fn a_keystone_is_narrower_at_the_top_than_at_the_bottom() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let size = 128;
        let quad = Quad::keystone(Rect::from_size(size as f64, size as f64), 32.0);
        let pixels = renderer
            .render_to_rgba(&gpu, &quadrants(), size, size, quad, 1.0)
            .unwrap();

        let opaque_in_row = |row: u32| {
            (0..size)
                .filter(|column| pixel(&pixels, size, *column, row)[3] > 128)
                .count()
        };

        let top = opaque_in_row(2);
        let bottom = opaque_in_row(size - 3);

        assert!(top > 0 && bottom > 0, "nothing was drawn at all");
        assert!(
            bottom > top + 40,
            "the keystone is not keystoned: {top} px at the top, {bottom} at the bottom"
        );
    }

    /// The property the CPU maths promises, checked against what the GPU
    /// actually drew: the picture's centre lands where the diagonals cross,
    /// which on a keystone is *not* the middle of the shape.
    #[test]
    fn the_centre_of_the_picture_lands_where_the_homography_says() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let size = 128;
        let quad = Quad::keystone(Rect::from_size(size as f64, size as f64), 40.0);

        // A picture that is red on the left and green on the right, so the
        // boundary between them marks the vertical centre line.
        let frame = Frame::packed(
            2,
            1,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::ZERO,
            vec![255, 0, 0, 255, 0, 255, 0, 255],
        )
        .unwrap();

        let pixels = renderer
            .render_to_rgba(&gpu, &frame, size, size, quad, 1.0)
            .unwrap();

        let map = quad.homography().unwrap();

        // Along three rows, the red/green boundary must sit where the map puts
        // u = 0.5. On a keystone that x moves from row to row.
        for row in [20_u32, 64, 110] {
            let expected = map
                .transform(Point::new(0.5, f64::from(row) / f64::from(size)))
                .x;

            let boundary = (0..size)
                .find(|column| {
                    let texel = pixel(&pixels, size, *column, row);
                    texel[3] > 128 && texel[1] > texel[0]
                })
                .map(f64::from);

            let Some(boundary) = boundary else {
                panic!("row {row} has no green half");
            };

            assert!(
                (boundary - expected).abs() < 3.0,
                "row {row}: the picture's midline is at {boundary:.1}, \
                 the homography says {expected:.1}"
            );
        }
    }

    #[test]
    fn opacity_reaches_the_alpha_channel() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let size = 32;
        let quad = Quad::from_rect(Rect::from_size(size as f64, size as f64));
        let pixels = renderer
            .render_to_rgba(&gpu, &quadrants(), size, size, quad, 0.5)
            .unwrap();

        let alpha = pixel(&pixels, size, 16, 16)[3];
        assert!((120..=136).contains(&alpha), "alpha came back {alpha}");
    }

    /// YUV goes to the GPU as planes and comes back as colour, with no CPU
    /// conversion anywhere in between.
    #[test]
    fn a_planar_yuv_frame_is_converted_by_the_shader() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        // Limited-range white: Y at 235, chroma neutral.
        let mut data = vec![235_u8; 4];
        data.extend_from_slice(&[128; 1]);
        data.extend_from_slice(&[128; 1]);

        let frame = Frame::packed(
            2,
            2,
            PixelFormat::I420,
            ColorSpace::default(),
            Duration::ZERO,
            data,
        )
        .unwrap();

        let size = 16;
        let quad = Quad::from_rect(Rect::from_size(size as f64, size as f64));
        let pixels = renderer
            .render_to_rgba(&gpu, &frame, size, size, quad, 1.0)
            .unwrap();

        let [red, green, blue, alpha] = pixel(&pixels, size, 8, 8);

        assert!(
            red > 250 && green > 250 && blue > 250,
            "{red} {green} {blue}"
        );
        assert_eq!(alpha, 255);
    }

    #[test]
    fn a_quad_that_folds_over_is_refused_rather_than_drawn() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let bowtie = Quad::new([
            Point::new(0.0, 0.0),
            Point::new(32.0, 0.0),
            Point::new(0.0, 32.0),
            Point::new(32.0, 32.0),
        ]);

        assert!(matches!(
            renderer.render_to_rgba(&gpu, &quadrants(), 32, 32, bowtie, 1.0),
            Err(Error::Placement { .. })
        ));
    }

    #[test]
    fn drawing_before_uploading_says_so_rather_than_showing_black() {
        let Some(gpu) = gpu() else { return };
        let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 16,
                height: 16,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let quad = Quad::from_rect(Rect::from_size(16.0, 16.0));

        assert!(matches!(
            renderer.draw(&gpu, &view, 16, 16, quad, 1.0),
            Err(Error::Render { .. })
        ));
    }

    #[test]
    fn ten_bit_says_what_is_missing_rather_than_drawing_noise() {
        assert!(matches!(
            mode_for(PixelFormat::P010),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn textures_are_reused_when_the_next_frame_is_the_same_shape() {
        let Some(gpu) = gpu() else { return };
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).unwrap();

        renderer.upload(&gpu, &quadrants()).unwrap();
        // Identity by address: wgpu handles are reference-counted, so the same
        // texture reallocated would be a different pointer.
        let first = std::ptr::addr_of!(renderer.planes.as_ref().unwrap().textures[0]) as usize;
        let first_width = renderer.planes.as_ref().unwrap().width;

        renderer.upload(&gpu, &quadrants()).unwrap();
        let second = std::ptr::addr_of!(renderer.planes.as_ref().unwrap().textures[0]) as usize;

        assert_eq!(
            first, second,
            "a same-shaped frame reallocated its textures"
        );
        assert_eq!(first_width, 2);

        // A different shape must replace them.
        let bigger = Frame::packed(
            4,
            4,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::ZERO,
            vec![255; 4 * 4 * 4],
        )
        .unwrap();

        renderer.upload(&gpu, &bigger).unwrap();
        assert_eq!(renderer.planes.as_ref().unwrap().width, 4);
    }
}
