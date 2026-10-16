//! 壁纸渲染器:wgpu 之上的一层薄封装,负责「一组参数 → 一帧画面」。
//!
//! 只有一条路:`render()` + `PhotoParams` —— 一张(生好的 / 抬进来的)图当纹理,
//! 着色器负责构图(cover 裁切 + Ken Burns 推拉平移)与叠加动效层。
//!
//! 上一代那条「纯程序化场景」的路(`SceneParams` + `scene.wgsl`)已经随**预设清零**退休:
//! 那种画面靠几个噪声函数现算,模型画出来的东西比它好看得多,留着只是两套并行的东西要一起维护。
//!
//! 取帧 / 提交 / 呈现那一整段每一行都是踩过坑换来的注释(见 `Dx12SwapchainKind` /
//! `latency_waitable_object` / 显式 `present`),所以全部收在私有的 `draw()` 里,只此一份。

use std::num::NonZeroIsize;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

/// 健康度上报用:当前进程 `(工作集 MB, 私有提交 MB)`。
fn mem_mb() -> (f64, f64) {
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok.as_bool() {
        (
            counters.WorkingSetSize as f64 / 1_048_576.0,
            counters.PagefileUsage as f64 / 1_048_576.0,
        )
    } else {
        (0.0, 0.0)
    }
}
use bytemuck::{Pod, Zeroable};
use mcwp_spec::{MotionKind, Photo};
use raw_window_handle::{RawWindowHandle, Win32WindowHandle};

/// 一个最小的 `HasDisplayHandle` 实现。
///
/// wgpu 30 起,DX12 后端创建 surface 时需要一个 display handle;我们的窗口是裸 Win32 窗口,
/// 没有 winit 之类的宿主,所以自己提供一个「Windows 桌面」display handle。
#[derive(Debug)]
struct WindowsDisplay(raw_window_handle::WindowsDisplayHandle);

impl raw_window_handle::HasDisplayHandle for WindowsDisplay {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(
                raw_window_handle::RawDisplayHandle::Windows(self.0),
            )
        })
    }
}

/// 照片壁纸着色器:一张静态图 + 时间轴 = 会动的壁纸(推拉、平移、效果层)。
pub const PHOTO_SHADER: &str = include_str!("photo.wgsl");

/// 照片纹理的格式。**必须是 sRGB**:生图存下来就是 sRGB 编码的 PNG,
/// 用 `Rgba8Unorm` 读会把 gamma 当线性处理,画面整体发灰发暗。
const PHOTO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn parse_hex(hex: &str) -> Option<[f32; 4]> {
    let s = hex.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([
        ((v >> 16) & 0xFF) as f32 / 255.0,
        ((v >> 8) & 0xFF) as f32 / 255.0,
        (v & 0xFF) as f32 / 255.0,
        1.0,
    ])
}

/// 传给照片着色器的参数,内存布局必须与 `photo.wgsl` 的 `Params` 完全一致。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct PhotoParams {
    /// 渲染分辨率(像素)
    pub resolution: [f32; 2],
    /// 累计时间(秒)
    pub time: f32,
    /// 亮度系数,取值区间 0.2 ~ 1.6
    pub brightness: f32,
    /// 从这张图里提出来的 5 色调色板(rgba)
    pub palette: [[f32; 4]; 5],
    /// Ken Burns 推近幅度 0~1
    pub zoom: f32,
    /// 平移幅度 0~1
    pub pan: f32,
    /// 速度 0~1
    pub speed: f32,
    /// 效果层强度 0~1
    pub intensity: f32,
    /// 动效编号,取值见 `effect_index()`
    pub effect: f32,
    /// 图的长宽比
    pub image_aspect: f32,
    /// 屏的长宽比
    pub screen_aspect: f32,
    /// 随机种子,决定效果层花纹落在哪
    pub seed: f32,
    /// 暗角强度 0~1
    pub vignette: f32,
    /// 颗粒感 0~1
    pub grain: f32,
    /// 补齐到 16 字节对齐(WGSL uniform 要求)
    pub _pad: [f32; 2],
}

impl PhotoParams {
    /// `Photo` + 屏幕尺寸 → 着色器参数。
    pub fn from_photo(photo: &Photo, width: u32, height: u32, desktop_brightness: f32) -> Self {
        let mut palette = [[0.0f32; 4]; 5];
        for (slot, hex) in palette.iter_mut().zip(photo.palette.iter()) {
            if let Some(rgb) = parse_hex(hex) {
                *slot = rgb;
            }
        }
        let motion = photo.motion;
        Self {
            resolution: [width as f32, height as f32],
            time: 0.0,
            brightness: desktop_brightness.clamp(0.2, 1.6),
            palette,
            zoom: motion.zoom,
            pan: motion.pan,
            speed: motion.speed,
            intensity: motion.intensity,
            effect: effect_index(motion.kind) as f32,
            image_aspect: photo.aspect(),
            screen_aspect: width as f32 / height.max(1) as f32,
            seed: photo.seed as f32,
            // 暗角给得比程序化场景轻:照片本身已经有构图和明暗,再压一圈会显得脏
            vignette: 0.55,
            grain: 0.12,
            _pad: [0.0; 2],
        }
    }
}

/// `MotionKind` → `photo.wgsl` 里那个 `effect` 编号。
///
/// 没写成 `match`:那个 `match` 和 `MotionKind::ALL` 的顺序是同一件事,写两遍迟早会漂。
/// 定死"编号 = `ALL` 里的下标",改枚举顺序时 WGSL 那边一起改一次就够了。
fn effect_index(kind: MotionKind) -> u32 {
    MotionKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(0) as u32
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// 管线:两组 bind —— group 0 是参数(uniform),group 1 是纹理 + 采样器(见 `photo.wgsl`)
    photo_pipeline: wgpu::RenderPipeline,
    photo_uniform: wgpu::Buffer,
    /// group 0:参数
    photo_bind: wgpu::BindGroup,
    /// group 1:纹理 + 采样器。换图 = 换它
    photo_texture_bind: wgpu::BindGroup,
    /// group 1 的布局与采样器。换图时只重建 bind group,这两个全程不变
    photo_texture_layout: wgpu::BindGroupLayout,
    photo_sampler: wgpu::Sampler,
    /// 纹理本体。**必须和 `photo_texture_bind` 一起活到同一个生命周期**:
    /// bind group 里的 texture view 是它的借用,纹理先没了 view 就悬空。
    photo_texture: wgpu::Texture,
    /// 跳过的帧数(被遮挡 / 超时 / 需要重新配置),用于健康度上报
    pub skipped_frames: u64,
    /// 上一帧的三个阶段耗时(毫秒):取帧 / 编码提交 / 呈现
    pub last_acquire_ms: f32,
    pub last_submit_ms: f32,
    pub last_present_ms: f32,
    /// 上一帧 `write_buffer` 的耗时(毫秒)
    pub last_write_ms: f32,
}

impl Renderer {
    /// 用原生窗口句柄创建渲染器(`hwnd` 是 Win32 HWND 的裸指针值)。
    pub fn new(hwnd: isize, width: u32, height: u32) -> Result<Self> {
        let (ws, _) = mem_mb();
        tracing::debug!("[内存] 进入 Renderer::new 时:工作集={ws:.1}MB");
        let handle = NonZeroIsize::new(hwnd).ok_or_else(|| anyhow!("无效的窗口句柄"))?;
        // 壁纸窗口是挂在 shell 窗口下面的子窗口,用 `CreateSwapChainForHwnd` 的直连路径
        // 在这类窗口上会「呈现成功但上不了屏」。这里改用 DirectComposition:
        // swapchain 走 `CreateSwapChainForComposition`,再由 `IDCompositionVisual` 贴到窗口上,
        // 这是 Chromium / WebView2 / mpv 嵌入渲染的标准做法。
        // 需要回退时用环境变量 `WGPU_DX12_PRESENTATION_SYSTEM=hwnd`。
        let backend_options = wgpu::BackendOptions {
            dx12: wgpu::Dx12BackendOptions {
                presentation_system: wgpu::Dx12SwapchainKind::DxgiFromVisual,
                // 帧延迟等待对象在本机 DX12 上是性能陷阱:`Wait` 会让每次取帧固定卡满
                // ~1 秒超时(实测 1fps),`None` 后稳定 59fps。壁纸不需要低延迟排队,写死。
                latency_waitable_object: wgpu::Dx12UseFrameLatencyWaitableObject::None,
                ..Default::default()
            },
            ..Default::default()
        }
        .with_env();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // 只走 DX12:Windows 上就够了,顺带避免把 Vulkan ICD 也加载进进程(白占内存)。
            backends: wgpu::Backends::DX12,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options,
            display: Some(Box::new(WindowsDisplay(
                raw_window_handle::WindowsDisplayHandle::new(),
            ))),
        });
        let (ws, _) = mem_mb();
        tracing::debug!("[内存] 建完 wgpu Instance 后:工作集={ws:.1}MB");

        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: None,
                raw_window_handle: RawWindowHandle::Win32(Win32WindowHandle::new(handle)),
            })
        }
        .context("为壁纸窗口创建 wgpu surface 失败")?;
        let (ws, _) = mem_mb();
        tracing::debug!("[内存] 建完 surface 后:工作集={ws:.1}MB");

        // 混合显卡机器上「用哪块 GPU 呈现」很关键:呈现用的适配器必须和显示器所属的适配器一致,
        // 否则交换链永远拿不到帧(表现就是 `get_current_texture` 每次卡满 1 秒超时、画面不刷新)。
        // 默认交给 wgpu 按低功耗挑,可用 `MCWP_GPU_INDEX` 指定。
        let wanted = std::env::var("MCWP_GPU_INDEX")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok());
        // 显式枚举适配器会为每块 GPU 建一套 DXGI/D3D12 工厂对象(本机实测多吃 ~35MB),
        // 所以只在真的要用 `MCWP_GPU_INDEX` 指定 GPU 时才枚举。
        let picked = wanted.and_then(|index| {
            let available =
                pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
            for (i, candidate) in available.iter().enumerate() {
                let info = candidate.get_info();
                tracing::info!(
                    "GPU[{i}] {} ({:?}, {:?}, surface={})",
                    info.name,
                    info.backend,
                    info.device_type,
                    candidate.is_surface_supported(&surface)
                );
            }
            available
                .into_iter()
                .nth(index)
                .filter(|candidate| candidate.is_surface_supported(&surface))
        });

        let adapter = match picked {
            Some(adapter) => adapter,
            None => pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
                apply_limit_buckets: false,
            }))
            .context("没有找到可用的 GPU 适配器")?,
        };
        let (ws, _) = mem_mb();
        tracing::debug!("[内存] 选定适配器后:工作集={ws:.1}MB");
        tracing::info!("使用 GPU:{:?}", adapter.get_info());

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("mcwp-device"),
            // 默认的 `Performance` 会让子分配器一上来就开 128~256MB 的显存堆,
            // 壁纸画面简单、资源极少,用 `Manual` 把设备块起点压到 4MB(宿主块自动取一半)。
            memory_hints: wgpu::MemoryHints::Manual {
                suballocated_device_memory_block_size: (4 << 20)..(16 << 20),
            },
            ..Default::default()
        }))
        .context("创建 wgpu 设备失败")?;
        let (ws, _) = mem_mb();
        tracing::debug!("[内存] 创建设备后:工作集={ws:.1}MB");

        // 交换链缓冲数 = desired_maximum_frame_latency + 1:默认 2 会让 DXGI 备 3 张
        // 1080p 缓冲(约 25MB),壁纸只要「最新一帧」,降到 1 即 2 张。
        let mut config = surface
            .get_default_config(&adapter, width.max(1), height.max(1))
            .ok_or_else(|| anyhow!("该 surface 不支持默认配置"))?;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        // group 0 的布局:一个 uniform(参数表)。只有一份 —— 照片那条路的
        // `photo_bind` 就是按它建的,换图时重新建的只有 group 1(纹理)。
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mcwp-params-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let photo_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mcwp-photo"),
            source: wgpu::ShaderSource::Wgsl(PHOTO_SHADER.into()),
        });
        let photo_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mcwp-photo-params"),
            size: std::mem::size_of::<PhotoParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let photo_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mcwp-photo-params-bind"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: photo_uniform.as_entire_binding(),
            }],
        });
        // 纹理那一路的布局只在这里建一次,换图时重建的只有 bind group(便宜)。
        let photo_texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mcwp-photo-texture-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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
        let photo_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("mcwp-photo-pipeline-layout"),
                bind_group_layouts: &[Some(&layout), Some(&photo_texture_layout)],
                immediate_size: 0,
            });
        let photo_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mcwp-photo-pipeline"),
            layout: Some(&photo_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &photo_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &photo_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        // 还没有照片时先挂一张 1×1 的占位纹理:这样 `render` 不需要处理
        // "纹理还没准备好"的分支 —— 它的正确用法是"先 set_photo 再每帧 render",
        // 万一顺序写反,屏幕上出现的是一块可认出来的占位色,而不是黑屏或 panic。
        let photo_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mcwp-photo-placeholder"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PHOTO_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &photo_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[24, 26, 32, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let photo_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mcwp-photo-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let photo_texture_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mcwp-photo-texture-bind"),
            layout: &photo_texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &photo_texture.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&photo_sampler),
                },
            ],
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            photo_pipeline,
            photo_uniform,
            photo_bind,
            photo_texture_bind,
            photo_texture_layout,
            photo_sampler,
            photo_texture,
            skipped_frames: 0,
            last_acquire_ms: 0.0,
            last_submit_ms: 0.0,
            last_present_ms: 0.0,
            last_write_ms: 0.0,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// 窗口尺寸变化后重新配置 surface。
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if width == self.config.width && height == self.config.height {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// GPU 允许的纹理最大边长。调用方拿它在**传图之前**把图缩到塞得下的尺寸 ——
    /// 不然只能等这里报错,那时候图早就解开了、内存也花掉了。
    pub fn max_texture_size(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// 换一张照片。旧纹理在这里直接丢掉(从下一帧起没人再引用它)。
    ///
    /// 参数收 `&[u8]` 而不是某个位图结构:渲染器**不该认识 PNG** —— 解码、缩放全在
    /// `mcwp-image`,这里只要"行优先、无 padding 的 RGBA 字节",这样渲染 crate 的依赖树
    /// 里永远不会有解码器。
    pub fn set_photo(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(anyhow!("照片尺寸是 0"));
        }
        let limit = self.max_texture_size();
        if width > limit || height > limit {
            return Err(anyhow!(
                "照片 {width}×{height} 超过这块 GPU 的上限 {limit},先在 `mcwp-image` 里缩一下"
            ));
        }
        let needed = width as usize * height as usize * 4;
        if rgba.len() < needed {
            return Err(anyhow!(
                "RGBA 字节不够:按 {width}×{height} 需要 {needed},实际只有 {}",
                rgba.len()
            ));
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mcwp-photo"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PHOTO_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // `write_texture` 没有 256 字节行对齐的要求(那是 `copy_buffer_to_texture` 的规矩),
        // 所以整块一次写进去就行,不用按行补齐 —— 1080p 上省掉一次 8 MB 的中转。
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba[..needed],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&Default::default());
        self.photo_texture_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mcwp-photo-texture-bind"),
            layout: &self.photo_texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.photo_sampler),
                },
            ],
        });
        self.photo_texture = texture;
        Ok(())
    }

    /// 渲染一帧壁纸。返回 `false` 表示这一帧被跳过(仍需继续循环)。
    ///
    /// **先 `set_photo` 再调它**:纹理还没换上去时,画出来的是一块可认出来的占位色。
    pub fn render(&mut self, params: &PhotoParams) -> Result<bool> {
        let t0 = Instant::now();
        self.queue
            .write_buffer(&self.photo_uniform, 0, bytemuck::bytes_of(params));
        let t_write = Instant::now();

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                self.skipped_frames += 1;
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.skipped_frames += 1;
                return Ok(false);
            }
        };
        let t1 = Instant::now();

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mcwp-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mcwp-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.photo_pipeline);
            pass.set_bind_group(0, &self.photo_bind, &[]);
            pass.set_bind_group(1, &self.photo_texture_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        let t2 = Instant::now();
        // wgpu 30 必须显式调用 `Queue::present`:`SurfaceTexture` 析构等于「丢弃这一帧」,
        // 不调用的话画面永远上不了屏(而且交换链很快就取不到帧,`get_current_texture` 每次都卡满超时)。
        self.queue.present(frame);
        let t3 = Instant::now();
        self.last_acquire_ms = (t1 - t0).as_secs_f32() * 1000.0;
        self.last_write_ms = (t_write - t0).as_secs_f32() * 1000.0;
        self.last_submit_ms = (t2 - t1).as_secs_f32() * 1000.0;
        self.last_present_ms = (t3 - t2).as_secs_f32() * 1000.0;
        Ok(true)
    }

    /// 自检:把当前壁纸渲染到**离屏**纹理再回读中心像素。
    ///
    /// 离屏渲染的 clear 色故意用洋红:回读到洋红说明「绘制没发生」(管线问题),
    /// 回读到画面本身说明「渲染是好的」(那问题就在呈现/合成环节)。
    pub fn readback_center(&mut self, params: &PhotoParams) -> Result<[u8; 4]> {
        let width = self.config.width;
        let height = self.config.height;
        let format = self.config.format;

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mcwp-selftest-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());

        self.queue
            .write_buffer(&self.photo_uniform, 0, bytemuck::bytes_of(params));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mcwp-selftest"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mcwp-selftest-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 1.0,
                            g: 0.0,
                            b: 1.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.photo_pipeline);
            pass.set_bind_group(0, &self.photo_bind, &[]);
            pass.set_bind_group(1, &self.photo_texture_bind, &[]);
            pass.draw(0..3, 0..1);
        }

        let bytes_per_pixel = 4u32;
        let padded_row = (width * bytes_per_pixel).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mcwp-selftest-readback"),
            size: (padded_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .map_err(|err| anyhow!("等待 GPU 失败:{err:?}"))?;
        receiver
            .recv()
            .map_err(|_| anyhow!("回读回调没有执行"))?
            .map_err(|err| anyhow!("映射回读缓冲失败:{err:?}"))?;

        let data = slice
            .get_mapped_range()
            .map_err(|err| anyhow!("读取回读缓冲失败:{err:?}"))?;
        let offset = (padded_row * (height / 2) + (width / 2) * bytes_per_pixel) as usize;
        let pixel = [data[offset], data[offset + 1], data[offset + 2], data[offset + 3]];
        drop(data);
        readback.unmap();
        Ok(pixel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcwp_spec::MotionRecipe;

    /// uniform 的字节布局是 Rust 和 WGSL 之间**唯一**没有编译器保护的约定:
    /// 这边挪一个字段、那边不改,画面不会报错,只会诡异地错位。所以在这里把偏移钉死。
    #[test]
    fn 照片参数的显存布局和着色器逐字段对齐() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<PhotoParams>(), 144);
        assert_eq!(offset_of!(PhotoParams, resolution), 0);
        assert_eq!(offset_of!(PhotoParams, time), 8);
        assert_eq!(offset_of!(PhotoParams, brightness), 12);
        assert_eq!(offset_of!(PhotoParams, palette), 16);
        assert_eq!(offset_of!(PhotoParams, zoom), 96);
        assert_eq!(offset_of!(PhotoParams, pan), 100);
        assert_eq!(offset_of!(PhotoParams, speed), 104);
        assert_eq!(offset_of!(PhotoParams, intensity), 108);
        assert_eq!(offset_of!(PhotoParams, effect), 112);
        assert_eq!(offset_of!(PhotoParams, image_aspect), 116);
        assert_eq!(offset_of!(PhotoParams, screen_aspect), 120);
        assert_eq!(offset_of!(PhotoParams, seed), 124);
        assert_eq!(offset_of!(PhotoParams, vignette), 128);
        assert_eq!(offset_of!(PhotoParams, grain), 132);
        assert_eq!(offset_of!(PhotoParams, _pad), 136);
    }

    /// 动效编号 = `MotionKind::ALL` 的下标,而 `photo.wgsl` 里的 `if (effect == n)` 就是这个
    /// 下标。枚举顺序一改、着色器不改,动效会整体串位(雪变成雨),所以在这里钉死。
    #[test]
    fn 动效编号和着色器里的分支一一对应() {
        assert_eq!(effect_index(MotionKind::Still), 0);
        assert_eq!(effect_index(MotionKind::Snow), 1);
        assert_eq!(effect_index(MotionKind::Rain), 2);
        assert_eq!(effect_index(MotionKind::Fog), 3);
        assert_eq!(effect_index(MotionKind::Cloud), 4);
        assert_eq!(effect_index(MotionKind::Glow), 5);
        assert_eq!(effect_index(MotionKind::Ripple), 6);
    }

    #[test]
    fn 照片参数把配方和比例原样带过去() {
        let mut photo = Photo {
            title: "雪山日出".into(),
            prompt: "雪山日出".into(),
            width: 1920,
            height: 1088,
            palette: vec![
                "#0B1020".into(),
                "#1B2A4A".into(),
                "#4F7CFF".into(),
                "#9BB8FF".into(),
                "#E6ECFF".into(),
            ],
            ..Default::default()
        };
        photo.motion = MotionRecipe {
            kind: MotionKind::Snow,
            intensity: 0.4,
            zoom: 0.2,
            pan: 0.1,
            speed: 0.25,
        };
        let params = PhotoParams::from_photo(&photo, 2560, 1440, 1.0);
        assert_eq!(params.effect, 1.0);
        assert_eq!(params.zoom, 0.2);
        assert_eq!(params.speed, 0.25);
        assert!((params.image_aspect - 1920.0 / 1088.0).abs() < 1e-6);
        assert!((params.screen_aspect - 2560.0 / 1440.0).abs() < 1e-6);
        // 调色板第一个槽是 #0B1020 → r=11/255
        assert!((params.palette[0][0] - 11.0 / 255.0).abs() < 1e-6);
    }
}

