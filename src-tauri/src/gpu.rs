//! Caminho de vídeo 100% na GPU (zero-copy):
//! captura (textura do Windows Graphics Capture) → textura compartilhada → Video Processor
//! (BGRA → NV12 + escala, na GPU) → encoder de hardware lendo direto da memória de vídeo.
//!
//! A captura usa o device D3D11 dela; o Media Foundation usa o nosso (com suporte a vídeo).
//! As texturas passam de um para o outro por handles compartilhados com keyed mutex.

use crate::media::{e, Res};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource};
use windows::Win32::Media::MediaFoundation::*;

const WAIT_TIMEOUT: i32 = 0x102;

/// AcquireSync com o HRESULT cru (o binding trata WAIT_TIMEOUT como sucesso).
fn acquire(km: &IDXGIKeyedMutex, key: u64, ms: u32) -> bool {
    unsafe {
        let hr: HRESULT = (Interface::vtable(km).AcquireSync)(Interface::as_raw(km), key, ms);
        hr.is_ok() && hr.0 != WAIT_TIMEOUT
    }
}

/* ========================================================================== */
/*                        Lado da captura: anel compartilhado                   */
/* ========================================================================== */

struct Slot {
    tex: ID3D11Texture2D,
    km: IDXGIKeyedMutex,
    handle: usize,
}

/// Três texturas compartilhadas no device da captura.
pub struct SharedRing {
    slots: Vec<Slot>,
    size: (u32, u32),
    next: usize,
}

/// Quadro na GPU pronto para o pipeline: handle compartilhado + tamanho.
pub struct GpuFrame {
    pub handle: usize,
    pub width: u32,
    pub height: u32,
}

impl SharedRing {
    pub fn new() -> Self {
        Self { slots: Vec::new(), size: (0, 0), next: 0 }
    }

    /// Copia a textura capturada para o próximo slot livre. `None` = todos ocupados (descarta).
    pub fn push(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D, w: u32, h: u32) -> Res<Option<GpuFrame>> {
        if self.size != (w, h) || self.slots.is_empty() {
            self.slots.clear();
            for _ in 0..3 {
                self.slots.push(create_shared(device, w, h)?);
            }
            self.size = (w, h);
        }
        for _ in 0..self.slots.len() {
            let i = self.next;
            self.next = (self.next + 1) % self.slots.len();
            let slot = &self.slots[i];
            if !acquire(&slot.km, 0, 0) {
                continue; // o encoder ainda está usando este slot
            }
            unsafe {
                let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                ctx.CopySubresourceRegion(&slot.tex, 0, 0, 0, 0, src, 0, Some(&region));
                let _ = slot.km.ReleaseSync(1);
            }
            return Ok(Some(GpuFrame { handle: slot.handle, width: w, height: h }));
        }
        Ok(None)
    }
}

fn create_shared(device: &ID3D11Device, w: u32, h: u32) -> Res<Slot> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
        };
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(e("textura compartilhada"))?;
        let tex = tex.ok_or("textura nula")?;
        let km: IDXGIKeyedMutex = tex.cast().map_err(e("keyed mutex"))?;
        let handle = tex.cast::<IDXGIResource>().map_err(e("IDXGIResource"))?.GetSharedHandle().map_err(e("GetSharedHandle"))?;
        Ok(Slot { tex, km, handle: handle.0 as usize })
    }
}

/* ========================================================================== */
/*                   Lado do pipeline: device com suporte a vídeo               */
/* ========================================================================== */

pub struct GpuDevice {
    pub device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
    opened: HashMap<usize, (ID3D11Texture2D, IDXGIKeyedMutex)>,
}

impl GpuDevice {
    pub fn new() -> Res<Self> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(e("D3D11CreateDevice"))?;
            let device = device.ok_or("device nulo")?;
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(e("MFCreateDXGIDeviceManager"))?;
            let manager = manager.ok_or("manager nulo")?;
            manager.ResetDevice(&device, token).map_err(e("ResetDevice"))?;
            let ctx = ctx.ok_or("contexto nulo")?;
            Ok(Self { device, ctx, manager, opened: HashMap::new() })
        }
    }

    /// Devolve à captura um quadro que não vai ser usado (foi substituído por um mais novo).
    pub fn discard(&mut self, frame: &GpuFrame) {
        if let Ok((_, km)) = self.open(frame.handle) {
            if acquire(&km, 1, 0) {
                unsafe { let _ = km.ReleaseSync(0); }
            }
        }
    }

    /// Abre (uma vez) a textura compartilhada no nosso device.
    fn open(&mut self, handle: usize) -> Res<(ID3D11Texture2D, IDXGIKeyedMutex)> {
        if let Some(t) = self.opened.get(&handle) {
            return Ok(t.clone());
        }
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device.OpenSharedResource(HANDLE(handle as *mut _), &mut tex).map_err(e("OpenSharedResource"))?;
            let tex = tex.ok_or("textura nula")?;
            let km: IDXGIKeyedMutex = tex.cast().map_err(e("keyed mutex"))?;
            if self.opened.len() > 12 {
                self.opened.clear(); // tamanho mudou várias vezes: descarta os antigos
            }
            self.opened.insert(handle, (tex.clone(), km.clone()));
            Ok((tex, km))
        }
    }
}

fn texture_sample(tex: &ID3D11Texture2D, time_hns: i64, dur_hns: i64) -> Res<IMFSample> {
    unsafe {
        let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false).map_err(e("MFCreateDXGISurfaceBuffer"))?;
        if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
            if let Ok(len) = b2.GetContiguousLength() {
                let _ = buf.SetCurrentLength(len);
            }
        }
        let sample = MFCreateSample().map_err(e("MFCreateSample"))?;
        sample.AddBuffer(&buf).map_err(e("AddBuffer"))?;
        sample.SetSampleTime(time_hns).map_err(e("SetSampleTime"))?;
        sample.SetSampleDuration(dur_hns).map_err(e("SetSampleDuration"))?;
        Ok(sample)
    }
}

/// Video Processor na GPU: textura BGRA → textura NV12 no tamanho de saída.
pub struct GpuConverter {
    mft: IMFTransform,
    /// Cópias comuns da entrada (o VP não aceita texturas com keyed mutex).
    inputs: Vec<ID3D11Texture2D>,
    next_in: usize,
    in_size: (u32, u32),
    out: (u32, u32),
    provides: bool,
    pool: Vec<ID3D11Texture2D>,
    next: usize,
}

impl GpuConverter {
    pub fn new(gpu: &GpuDevice, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Res<Self> {
        unsafe {
            let mft: IMFTransform = windows::Win32::System::Com::CoCreateInstance(
                &CLSID_VideoProcessorMFT,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )
            .map_err(e("VideoProcessorMFT"))?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize).map_err(e("VP SET_D3D_MANAGER"))?;
            let input = crate::media::video_type(&MFVideoFormat_ARGB32, in_w, in_h, fps)?;
            mft.SetInputType(0, &input, 0).map_err(e("VP GPU SetInputType"))?;
            let output = crate::media::video_type(&MFVideoFormat_NV12, out_w, out_h, fps)?;
            mft.SetOutputType(0, &output, 0).map_err(e("VP GPU SetOutputType"))?;
            // As texturas que o VP aloca precisam servir de alvo de render e de entrada do encoder.
            if let Ok(attrs) = mft.GetOutputStreamAttributes(0) {
                let flags = D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0;
                attrs.SetUINT32(&MF_SA_D3D11_BINDFLAGS, flags as u32).ok();
            }
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            let info = mft.GetOutputStreamInfo(0).map_err(e("VP stream info"))?;
            // Só deixa o VP alocar quando ele exige; senão usamos nossas texturas NV12.
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            // Nossas texturas NV12: a saída do VP é copiada para cá e devolvida na hora
            // (o estoque interno do VP é pequeno e esgotaria com o encoder segurando quadros).
            let mut pool = Vec::new();
            for _ in 0..6 {
                pool.push(nv12_texture(&gpu.device, out_w, out_h)?);
            }
            let mut inputs = Vec::new();
            for _ in 0..3 {
                inputs.push(bgra_texture(&gpu.device, in_w, in_h)?);
            }
            Ok(Self { mft, inputs, next_in: 0, in_size: (in_w, in_h), out: (out_w, out_h), provides, pool, next: 0 })
        }
    }

    pub fn input_size(&self) -> (u32, u32) {
        self.in_size
    }

    pub fn output_size(&self) -> (u32, u32) {
        self.out
    }

    /// Converte o quadro compartilhado `frame` (aberto no nosso device) em NV12.
    pub fn convert(&mut self, gpu: &mut GpuDevice, frame: &GpuFrame, time_hns: i64, dur_hns: i64) -> Res<IMFSample> {
        let (shared, km) = gpu.open(frame.handle)?;
        if !acquire(&km, 1, 100) {
            return Err("quadro da GPU indisponível".into());
        }
        // Cópia GPU→GPU para uma textura comum e libera o slot para a captura na hora.
        let tex = self.inputs[self.next_in].clone();
        self.next_in = (self.next_in + 1) % self.inputs.len();
        unsafe {
            gpu.ctx.CopyResource(&tex, &shared);
            let _ = km.ReleaseSync(0);
        }
        let result = (|| -> Res<IMFSample> {
            let input = texture_sample(&tex, time_hns, dur_hns)?;
            unsafe { self.mft.ProcessInput(0, &input, 0).map_err(e("VP GPU ProcessInput"))? };
            let mut last = None;
            loop {
                let out_sample = if self.provides {
                    None
                } else {
                    let t = &self.pool[self.next];
                    self.next = (self.next + 1) % self.pool.len();
                    Some(texture_sample(t, time_hns, dur_hns)?)
                };
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(out_sample),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
                let s = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
                unsafe { drop(ManuallyDrop::take(&mut buffers[0].pEvents)) };
                match r {
                    Ok(()) => last = s,
                    Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    // Com amostras próprias, o VP recusa a segunda leitura em vez de pedir entrada.
                    Err(_) if last.is_some() => break,
                    Err(err) => return Err(format!("VP GPU ProcessOutput: {err} ({:#x})", err.code().0)),
                }
            }
            let vp_out = last.ok_or("VP GPU sem saída")?;
            if !self.provides {
                unsafe {
                    vp_out.SetSampleTime(time_hns).ok();
                    vp_out.SetSampleDuration(dur_hns).ok();
                }
                return Ok(vp_out);
            }
            // Copia para uma textura nossa e solta a amostra do VP.
            let src: ID3D11Texture2D = unsafe {
                let buf = vp_out.GetBufferByIndex(0).map_err(e("VP buffer"))?;
                let dxgi: IMFDXGIBuffer = buf.cast().map_err(e("IMFDXGIBuffer"))?;
                let mut ptr = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut ptr).map_err(e("GetResource"))?;
                let sub = dxgi.GetSubresourceIndex().unwrap_or(0);
                let tex = ID3D11Texture2D::from_raw(ptr);
                let dst = self.pool[self.next].clone();
                self.next = (self.next + 1) % self.pool.len();
                gpu.ctx.CopySubresourceRegion(&dst, 0, 0, 0, 0, &tex, sub, None);
                drop(tex);
                dst
            };
            drop(vp_out);
            texture_sample(&src, time_hns, dur_hns)
        })();
        result
    }
}

fn bgra_texture(device: &ID3D11Device, w: u32, h: u32) -> Res<ID3D11Texture2D> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(e("textura BGRA"))?;
        tex.ok_or_else(|| "textura BGRA nula".into())
    }
}

fn nv12_texture(device: &ID3D11Device, w: u32, h: u32) -> Res<ID3D11Texture2D> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(e("textura NV12"))?;
        tex.ok_or_else(|| "textura NV12 nula".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn gpu_convert_isolated() {
        crate::media::mf_init().unwrap();
        let mut gpu = GpuDevice::new().expect("device");
        // Textura compartilhada criada no próprio device (simula a captura).
        let slot = create_shared(&gpu.device, 1920, 1080).expect("shared");
        assert!(acquire(&slot.km, 0, 0));
        unsafe { let _ = slot.km.ReleaseSync(1); }
        let mut conv = GpuConverter::new(&gpu, 1920, 1080, 1280, 720, 60).expect("conv");
        println!("VP provides samples: {}", conv.provides);
        let frame = GpuFrame { handle: slot.handle, width: 1920, height: 1080 };
        match conv.convert(&mut gpu, &frame, 0, 166_667) {
            Ok(_) => println!("conversão OK"),
            Err(e) => panic!("falhou: {e}"),
        }
    }

    #[test]
    #[ignore]
    fn gpu_convert_plain_texture() {
        crate::media::mf_init().unwrap();
        let gpu = GpuDevice::new().expect("device");
        let conv = GpuConverter::new(&gpu, 1920, 1080, 1280, 720, 60).expect("conv");
        for (name, misc, bind) in [
            ("comum", 0u32, (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32),
            ("só shader", 0u32, D3D11_BIND_SHADER_RESOURCE.0 as u32),
            ("keyed mutex", D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32, (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32),
        ] {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 1920, Height: 1080, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind, CPUAccessFlags: 0, MiscFlags: misc,
            };
            let mut tex = None;
            unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap() };
            let tex = tex.unwrap();
            if misc != 0 {
                let km: IDXGIKeyedMutex = tex.cast().unwrap();
                assert!(acquire(&km, 0, 0));
            }
            let input = texture_sample(&tex, 0, 166_667).unwrap();
            let r_in = unsafe { conv.mft.ProcessInput(0, &input, 0) };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0u32;
            let r_out = unsafe { conv.mft.ProcessOutput(0, &mut buffers, &mut status) };
            unsafe { drop(ManuallyDrop::take(&mut buffers[0].pSample)); drop(ManuallyDrop::take(&mut buffers[0].pEvents)); }
            println!("{name}: input {:?} output {:?}", r_in.map_err(|e| e.code().0), r_out.map_err(|e| format!("{:#x}", e.code().0)));
        }
    }

    /// Bitrate no caminho da GPU com conteúdo em movimento (deve ficar perto do alvo).
    #[test]
    #[ignore]
    fn gpu_bitrate_accuracy() {
        crate::media::mf_init().unwrap();
        let mut gpu = GpuDevice::new().expect("device");
        let (w, h, fps) = (1920u32, 1080u32, 60u32);
        for target in [6_000_000u32, 12_000_000, 20_000_000] {
        let slot = create_shared(&gpu.device, w, h).unwrap();
        let mut conv = GpuConverter::new(&gpu, w, h, w, h, fps).unwrap();
        let mut enc = crate::media::Encoder::new(w, h, fps, target, true, Some(&gpu.manager)).expect("encoder");
        let frame = GpuFrame { handle: slot.handle, width: w, height: h };
        let mut img = vec![0u8; (w * h * 4) as usize];
        let mut bytes = 0usize;
        let frames = 240u32;
        for i in 0..frames {
            for (j, px) in img.chunks_exact_mut(4).enumerate() {
                let (x, y) = (j as u32 % w, j as u32 / w);
                px[0] = ((x + i * 9) ^ (y * 3)) as u8; px[1] = ((y + i * 5) ^ x) as u8; px[2] = (i * 3) as u8; px[3] = 255;
            }
            assert!(acquire(&slot.km, 0, 1000));
            unsafe {
                gpu.ctx.UpdateSubresource(&slot.tex, 0, None, img.as_ptr() as *const _, w * 4, 0);
                let _ = slot.km.ReleaseSync(1);
            }
            let t = i as i64 * 10_000_000 / fps as i64;
            let nv12 = conv.convert(&mut gpu, &frame, t, 10_000_000 / fps as i64).unwrap();
            for p in enc.encode(&nv12).unwrap() { bytes += p.data.len(); }
        }
        let secs = frames as f64 / fps as f64;
        let mbps = bytes as f64 * 8.0 / secs / 1e6;
        println!("GPU: alvo {:.0} Mb/s, obtido {:.2} Mb/s", target as f64 / 1e6, mbps);
        }
    }

    #[test]
    #[ignore]
    fn cpu_bitrate_same_content() {
        crate::media::mf_init().unwrap();
        let (w, h, fps) = (1920u32, 1080u32, 60u32);
        for target in [6_000_000u32, 12_000_000, 20_000_000] {
            let conv = crate::media::Converter::new(w, h, w, h, fps).unwrap();
            let mut enc = crate::media::Encoder::new(w, h, fps, target, true, None).unwrap();
            let mut img = vec![0u8; (w * h * 4) as usize];
            let mut bytes = 0usize;
            let frames = 240u32;
            for i in 0..frames {
                for (j, px) in img.chunks_exact_mut(4).enumerate() {
                    let (x, y) = (j as u32 % w, j as u32 / w);
                    px[0] = ((x + i * 9) ^ (y * 3)) as u8; px[1] = ((y + i * 5) ^ x) as u8; px[2] = (i * 3) as u8; px[3] = 255;
                }
                let t = i as i64 * 10_000_000 / fps as i64;
                let nv12 = conv.convert(&img, t, 10_000_000 / fps as i64).unwrap();
                for p in enc.encode(&nv12).unwrap() { bytes += p.data.len(); }
            }
            println!("CPU: alvo {:.0} Mb/s, obtido {:.2} Mb/s", target as f64 / 1e6, bytes as f64 * 8.0 / (frames as f64 / fps as f64) / 1e6);
        }
    }
}
