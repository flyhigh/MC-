//! 壁纸渲染器:wgpu 之上的一层薄封装,负责「一份 SceneParams → 一帧画面」。
//!
//! M0 只有一个内置场景(雨夜色调渐变),但接口按最终形态设计:
//! 之后 WallpaperSpec 通过校验后,直接映射成 `SceneParams` 喂进来即可。

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
use mcwp_spec::{Preset, WallpaperSpec};
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

/// 内置场景着色器。
pub const SCENE_SHADER: &str = include_str!("scene.wgsl");

/// 传给着色器的场景参数,内存布局必须与 `scene.wgsl` 的 `Params` 完全一致。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct SceneParams {
    /// 渲染分辨率(像素)
    pub resolution: [f32; 2],
    /// 累计时间(秒)
    pub time: f32,
    /// 亮度系数,建议 0.2 ~ 1.6
    pub brightness: f32,
    /// 5 色调色板(rgba,alpha 保留)
    pub palette: [[f32; 4]; 5],
    /// 动势 0 ~ 1
    pub motion: f32,
    /// 强度 0 ~ 1
    pub intensity: f32,
    /// 动势速度系数 0 ~ 1
    pub speed: f32,
    /// 颗粒感 0 ~ 1(纸感场景用它当纸纤维)
    pub grain: f32,
    /// 场景自由参数 A(雨量 / 暖度 / 云密度 / 网格清晰度)
    pub density: f32,
    /// 场景自由参数 B(雾面 / 柔度 / 亮核 / 对比)
    pub softness: f32,
    /// 主光强度 0 ~ 1
    pub light_key: f32,
    /// 场景自由参数 C(视差 / 漩涡 / 扫描线)
    pub accent: f32,
    /// 暗角强度 0 ~ 1
    pub vignette: f32,
    /// 随机种子,保证同一份 spec 永远渲染出同一张画面
    pub seed: f32,
    /// 场景编号,对应 `Preset::scene_index()`
    pub scene: u32,
    /// 补齐到 16 字节对齐(WGSL uniform 要求)
    pub _pad: f32,
}

impl SceneParams {
    /// spec → 着色器参数:全项目唯一的「语义 → 画面槽位」翻译层。
    ///
    /// 分工:spec 负责「像什么」(预设 + 参数 + 配色),这里把情绪与亮度折算成具体系数。
    /// 着色器本身不认识 spec,只认这些槽位。
    pub fn from_spec(
        spec: &WallpaperSpec,
        width: u32,
        height: u32,
        desktop_brightness: f32,
    ) -> Self {
        let preset = spec.preset();
        let mood = spec.mood;

        // 每个预设声明三个自由参数,统一映射到 density / softness / accent 三个槽位。
        let (density, softness, accent) = match preset {
            Preset::RainyWindowCity => (
                spec.param("rain"),
                spec.param("blur"),
                spec.param("parallax"),
            ),
            Preset::MinimalPaper => (spec.param("warmth"), spec.param("softness"), 0.0),
            Preset::NebulaDrift => (
                spec.param("density"),
                spec.param("glow"),
                spec.param("swirl"),
            ),
            Preset::FocusGrid => (
                spec.param("contrast"),
                spec.param("grid"),
                spec.param("scan"),
            ),
        };

        // 颗粒:纸感场景用 spec 里的纸纤维强度,其余场景用它压掉渐变色带。
        let grain = match preset {
            Preset::MinimalPaper => spec.param("grain"),
            _ => 0.20 + 0.45 * (1.0 - mood.calm),
        };

        // 暗角:纸感要「干净」,所以几乎不打暗角。
        let vignette = match preset {
            Preset::MinimalPaper => 0.22,
            Preset::FocusGrid => 0.70,
            _ => 0.85,
        };

        let mut palette = [[0.0f32; 4]; 5];
        for (slot, hex) in palette.iter_mut().zip(spec.palette.iter()) {
            if let Some(rgb) = parse_hex(hex) {
                *slot = rgb;
            }
        }

        Self {
            resolution: [width as f32, height as f32],
            time: 0.0,
            brightness: desktop_brightness.clamp(0.2, 1.6),
            palette,
            motion: spec.motion.intensity,
            // 能量高 → 光感更足;专注高 → 收敛一点。
            intensity: (0.55 + 0.45 * mood.energy - 0.15 * mood.focus).clamp(0.2, 1.4),
            speed: spec.motion.speed,
            grain,
            density,
            softness,
            light_key: spec.lighting.key_light,
            accent,
            vignette,
            seed: spec.seed as f32,
            scene: preset.scene_index(),
            _pad: 0.0,
        }
    }
}

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

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
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

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mcwp-scene"),
            source: wgpu::ShaderSource::Wgsl(SCENE_SHADER.into()),
        });

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mcwp-scene-params"),
            size: std::mem::size_of::<SceneParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mcwp-scene-layout"),
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

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mcwp-scene-bind"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mcwp-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mcwp-scene-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
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

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            uniform,
            bind_group,
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

    /// 渲染一帧。返回 `false` 表示这一帧被跳过(仍需继续循环)。
    pub fn render(&mut self, params: &SceneParams) -> Result<bool> {
        let t0 = Instant::now();
        self.queue
            .write_buffer(&self.uniform, 0, bytemuck::bytes_of(params));
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
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

    /// 自检:把同一个场景渲染到**离屏**纹理再回读中心像素。
    ///
    /// 离屏渲染的 clear 色故意用洋红:回读到洋红说明「绘制没发生」(管线问题),
    /// 回读到深蓝渐变说明「渲染是好的」(那问题就在呈现/合成环节)。
    pub fn readback_center(&mut self, params: &SceneParams) -> Result<[u8; 4]> {
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
            .write_buffer(&self.uniform, 0, bytemuck::bytes_of(params));

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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
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

