//! NVIDIA Optical Flow (NVOFA): o acelerador de fluxo óptico das placas NVIDIA a partir
//! da geração Turing (GTX 16xx / RTX 20xx em diante). Calcula, por bloco de 4x4 pixels,
//! para onde a imagem se moveu entre dois quadros. É usado para gerar quadros
//! intermediários (ver `framegen`).
//!
//! Tudo vem do driver da NVIDIA (`nvcuda.dll` e `nvofapi64.dll`), carregado em tempo de
//! execução: sem placa NVIDIA compatível, `OpticalFlow::available()` é falso e nada muda.
//! Usamos a interface CUDA da API (o quadro decodificado já está na CPU; só o luma sobe).
//!
//! As definições abaixo seguem os headers públicos `nvOpticalFlowCommon.h` e
//! `nvOpticalFlowCuda.h` (API 2.0) de https://github.com/NVIDIA/NVIDIAOpticalFlowSDK:
//!
//! Copyright(c) 2020, NVIDIA CORPORATION. All rights reserved.
//! Redistribution and use in source and binary forms, with or without modification, are
//! permitted provided that the following conditions are met: 1. Redistributions of source
//! code must retain the above copyright notice, this list of conditions and the following
//! disclaimer. 2. Redistributions in binary form must reproduce the above copyright
//! notice, this list of conditions and the following disclaimer in the documentation
//! and/or other materials provided with the distribution. 3. Neither the name of the
//! copyright holder nor the names of its contributors may be used to endorse or promote
//! products derived from this software without specific prior written permission.
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY
//! EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF
//! MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL
//! THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//! SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT
//! OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
//! INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT,
//! STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF
//! THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#![allow(non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::sync::OnceLock;
use windows::core::{s, w, PCSTR};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type Res<T> = Result<T, String>;

/* --------------------------------------------------------------- CUDA (driver) */

type CUresult = i32;
type CUcontext = *mut c_void;
type CUdeviceptr = u64;
const CU_MEMORYTYPE_HOST: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;

#[repr(C)]
#[derive(Default)]
struct CUDA_MEMCPY2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: usize,
    src_device: CUdeviceptr,
    src_array: usize,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: usize,
    dst_device: CUdeviceptr,
    dst_array: usize,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

struct Cuda {
    init: unsafe extern "system" fn(u32) -> CUresult,
    device_get: unsafe extern "system" fn(*mut i32, i32) -> CUresult,
    ctx_create: unsafe extern "system" fn(*mut CUcontext, u32, i32) -> CUresult,
    ctx_destroy: unsafe extern "system" fn(CUcontext) -> CUresult,
    ctx_push: unsafe extern "system" fn(CUcontext) -> CUresult,
    ctx_pop: unsafe extern "system" fn(*mut CUcontext) -> CUresult,
    memcpy_2d: unsafe extern "system" fn(*const CUDA_MEMCPY2D) -> CUresult,
}

/* -------------------------------------------------------------------- NVOF API */

const NV_OF_API_VERSION: u32 = (2 << 4) | 0;
type NV_OF_STATUS = i32;
const NV_OF_SUCCESS: NV_OF_STATUS = 0;
type NvOFHandle = *mut c_void;
type NvOFGPUBufferHandle = *mut c_void;

const NV_OF_FALSE: u32 = 0;
const NV_OF_PERF_LEVEL_MEDIUM: u32 = 10;
const NV_OF_OUTPUT_VECTOR_GRID_SIZE_4: u32 = 4;
const NV_OF_HINT_VECTOR_GRID_SIZE_UNDEFINED: u32 = 0;
const NV_OF_MODE_OPTICALFLOW: u32 = 1;
const NV_OF_BUFFER_USAGE_INPUT: u32 = 1;
const NV_OF_BUFFER_USAGE_OUTPUT: u32 = 2;
const NV_OF_BUFFER_FORMAT_GRAYSCALE8: u32 = 1;
const NV_OF_BUFFER_FORMAT_SHORT2: u32 = 5;
const NV_OF_CUDA_BUFFER_TYPE_CUDEVICEPTR: u32 = 2;

#[repr(C)]
struct NV_OF_INIT_PARAMS {
    width: u32,
    height: u32,
    out_grid_size: u32,
    hint_grid_size: u32,
    mode: u32,
    perf_level: u32,
    enable_external_hints: u32,
    enable_output_cost: u32,
    h_priv_data: *mut c_void,
    disparity_range: u32,
    enable_roi: u32,
}

#[repr(C)]
struct NV_OF_BUFFER_DESCRIPTOR {
    width: u32,
    height: u32,
    buffer_usage: u32,
    buffer_format: u32,
}

#[repr(C)]
struct NV_OF_EXECUTE_INPUT_PARAMS {
    input_frame: NvOFGPUBufferHandle,
    reference_frame: NvOFGPUBufferHandle,
    external_hints: NvOFGPUBufferHandle,
    disable_temporal_hints: u32,
    padding: u32,
    h_priv_data: *mut c_void,
    padding2: u32,
    num_rois: u32,
    roi_data: *mut c_void,
}

#[repr(C)]
struct NV_OF_EXECUTE_OUTPUT_PARAMS {
    output_buffer: NvOFGPUBufferHandle,
    output_cost_buffer: NvOFGPUBufferHandle,
    h_priv_data: *mut c_void,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NV_OF_BUFFER_STRIDE {
    stride_x_in_bytes: u32,
    stride_y_in_bytes: u32,
}

#[repr(C)]
#[derive(Default)]
struct NV_OF_CUDA_BUFFER_STRIDE_INFO {
    stride_info: [NV_OF_BUFFER_STRIDE; 3],
    num_planes: u32,
}

#[repr(C)]
#[derive(Default)]
struct NV_OF_CUDA_API_FUNCTION_LIST {
    create: Option<unsafe extern "system" fn(CUcontext, *mut NvOFHandle) -> NV_OF_STATUS>,
    init: Option<unsafe extern "system" fn(NvOFHandle, *const NV_OF_INIT_PARAMS) -> NV_OF_STATUS>,
    create_buffer: Option<unsafe extern "system" fn(NvOFHandle, *const NV_OF_BUFFER_DESCRIPTOR, u32, *mut NvOFGPUBufferHandle) -> NV_OF_STATUS>,
    buffer_get_cuarray: Option<unsafe extern "system" fn(NvOFGPUBufferHandle) -> *mut c_void>,
    buffer_get_devptr: Option<unsafe extern "system" fn(NvOFGPUBufferHandle) -> CUdeviceptr>,
    buffer_get_stride: Option<unsafe extern "system" fn(NvOFGPUBufferHandle, *mut NV_OF_CUDA_BUFFER_STRIDE_INFO) -> NV_OF_STATUS>,
    set_io_streams: Option<unsafe extern "system" fn(NvOFHandle, *mut c_void, *mut c_void) -> NV_OF_STATUS>,
    execute: Option<unsafe extern "system" fn(NvOFHandle, *const NV_OF_EXECUTE_INPUT_PARAMS, *mut NV_OF_EXECUTE_OUTPUT_PARAMS) -> NV_OF_STATUS>,
    destroy_buffer: Option<unsafe extern "system" fn(NvOFGPUBufferHandle) -> NV_OF_STATUS>,
    destroy: Option<unsafe extern "system" fn(NvOFHandle) -> NV_OF_STATUS>,
    get_last_error: Option<unsafe extern "system" fn(NvOFHandle, *mut u8, *mut u32) -> NV_OF_STATUS>,
    get_caps: Option<unsafe extern "system" fn(NvOFHandle, u32, *mut u32, *mut u32) -> NV_OF_STATUS>,
}

struct Api {
    cu: Cuda,
    of: NV_OF_CUDA_API_FUNCTION_LIST,
}

// Ponteiros de função para DLLs que ficam carregadas até o fim do processo.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn load() -> Res<Api> {
    unsafe {
        let cuda = LoadLibraryW(w!("nvcuda.dll")).map_err(|_| "sem driver CUDA da NVIDIA")?;
        let nvof = LoadLibraryW(w!("nvofapi64.dll")).map_err(|_| "o driver não tem a NVIDIA Optical Flow")?;
        macro_rules! sym {
            ($lib:expr, $name:expr) => {{
                let name: PCSTR = $name;
                std::mem::transmute(GetProcAddress($lib, name).ok_or_else(|| format!("função ausente no driver: {}", name.display()))?)
            }};
        }
        let cu = Cuda {
            init: sym!(cuda, s!("cuInit")),
            device_get: sym!(cuda, s!("cuDeviceGet")),
            ctx_create: sym!(cuda, s!("cuCtxCreate_v2")),
            ctx_destroy: sym!(cuda, s!("cuCtxDestroy_v2")),
            ctx_push: sym!(cuda, s!("cuCtxPushCurrent_v2")),
            ctx_pop: sym!(cuda, s!("cuCtxPopCurrent_v2")),
            memcpy_2d: sym!(cuda, s!("cuMemcpy2D_v2")),
        };
        let max_version: unsafe extern "system" fn(*mut u32) -> NV_OF_STATUS = sym!(nvof, s!("NvOFGetMaxSupportedApiVersion"));
        let create_instance: unsafe extern "system" fn(u32, *mut NV_OF_CUDA_API_FUNCTION_LIST) -> NV_OF_STATUS =
            sym!(nvof, s!("NvOFAPICreateInstanceCuda"));
        let mut version = 0u32;
        if max_version(&mut version) != NV_OF_SUCCESS || version < NV_OF_API_VERSION {
            return Err(format!("driver com Optical Flow antiga (API {version:#x})"));
        }
        if (cu.init)(0) != 0 {
            return Err("CUDA não iniciou (sem placa NVIDIA ativa?)".into());
        }
        let mut of = NV_OF_CUDA_API_FUNCTION_LIST::default();
        if create_instance(NV_OF_API_VERSION, &mut of) != NV_OF_SUCCESS {
            return Err("NvOFAPICreateInstanceCuda falhou".into());
        }
        Ok(Api { cu, of })
    }
}

fn api() -> Res<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(|e| e.clone())
}

/* ------------------------------------------------------------------ interface */

/// Campo de vetores de movimento: um vetor por bloco de `grid` x `grid` pixels, em
/// pixels (formato S10.5 da NVIDIA: dividir por 32). `data` = [x, y, x, y, ...].
#[derive(Clone)]
pub struct FlowField {
    pub width: u32,
    pub height: u32,
    pub grid: u32,
    pub data: Vec<i16>,
}

impl FlowField {
    /// Vetor (em pixels) do bloco que contém o pixel (x, y).
    pub fn at(&self, x: f32, y: f32) -> (f32, f32) {
        let bx = ((x / self.grid as f32) as i64).clamp(0, self.width as i64 - 1) as usize;
        let by = ((y / self.grid as f32) as i64).clamp(0, self.height as i64 - 1) as usize;
        let i = (by * self.width as usize + bx) * 2;
        (self.data[i] as f32 / 32.0, self.data[i + 1] as f32 / 32.0)
    }
}

/// Movimento entre o quadro anterior (A) e o novo (B), nos dois sentidos.
pub struct FlowPair {
    /// Para cada bloco de A: onde ele está em B (A(p) ≈ B(p + v)).
    pub forward: FlowField,
    /// Para cada bloco de B: onde ele estava em A (B(p) ≈ A(p + v)).
    pub backward: FlowField,
}

impl FlowPair {
    /// Fração dos blocos em que ida e volta não batem (> 3 px). Alta numa troca de cena
    /// ou num movimento que o fluxo não acompanhou: aí é melhor não interpolar.
    pub fn inconsistency(&self) -> f32 {
        let f = &self.forward;
        let g = f.grid as f32;
        let (mut bad, mut total) = (0u32, 0u32);
        for by in (0..f.height).step_by(2) {
            for bx in (0..f.width).step_by(2) {
                let (x, y) = ((bx as f32 + 0.5) * g, (by as f32 + 0.5) * g);
                let (fx, fy) = f.at(x, y);
                let (bx2, by2) = self.backward.at(x + fx, y + fy);
                if (fx + bx2).hypot(fy + by2) > 3.0 {
                    bad += 1;
                }
                total += 1;
            }
        }
        bad as f32 / total.max(1) as f32
    }
}

struct Buffer {
    handle: NvOFGPUBufferHandle,
    ptr: CUdeviceptr,
    stride: usize,
}

/// Uma sessão do NVOF num sentido, com dois buffers de entrada alternados.
struct Session {
    handle: NvOFHandle,
    inputs: [Buffer; 2],
    output: Buffer,
}

/// Fluxo óptico entre quadros consecutivos (só o luma importa).
pub struct OpticalFlow {
    api: &'static Api,
    ctx: CUcontext,
    width: u32,
    height: u32,
    out_w: u32,
    out_h: u32,
    /// Ida (anterior → novo) e volta (novo → anterior). Sessões separadas: cada uma usa
    /// o próprio resultado anterior como dica (temporal hints), no mesmo sentido.
    forward: Session,
    backward: Session,
    /// Buffer de entrada com o quadro mais novo (0 ou 1); None = nenhum ainda.
    newest: Option<usize>,
}

// O contexto CUDA é empilhado/desempilhado a cada uso; a struct pode mudar de thread.
unsafe impl Send for OpticalFlow {}

impl OpticalFlow {
    /// Há placa NVIDIA com Optical Flow e driver compatível? (cacheado)
    pub fn available() -> bool {
        static OK: OnceLock<bool> = OnceLock::new();
        *OK.get_or_init(|| match OpticalFlow::new(256, 128) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("NVIDIA Optical Flow indisponível: {e}");
                false
            }
        })
    }

    pub fn new(width: u32, height: u32) -> Res<Self> {
        let api = api()?;
        let mut device = 0i32;
        let mut ctx: CUcontext = std::ptr::null_mut();
        unsafe {
            check_cu((api.cu.device_get)(&mut device, 0), "cuDeviceGet")?;
            check_cu((api.cu.ctx_create)(&mut ctx, 0, device), "cuCtxCreate")?;
        }
        let mut of = OpticalFlow {
            api,
            ctx,
            width,
            height,
            out_w: width.div_ceil(NV_OF_OUTPUT_VECTOR_GRID_SIZE_4),
            out_h: height.div_ceil(NV_OF_OUTPUT_VECTOR_GRID_SIZE_4),
            forward: Session { handle: std::ptr::null_mut(), inputs: [Buffer::null(), Buffer::null()], output: Buffer::null() },
            backward: Session { handle: std::ptr::null_mut(), inputs: [Buffer::null(), Buffer::null()], output: Buffer::null() },
            newest: None,
        };
        // Em caso de erro, o Drop libera o que já foi criado.
        of.with_ctx(|of| {
            of.forward = of.session()?;
            of.backward = of.session()?;
            Ok(())
        })?;
        Ok(of)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn with_ctx<T>(&mut self, f: impl FnOnce(&mut Self) -> Res<T>) -> Res<T> {
        unsafe { check_cu((self.api.cu.ctx_push)(self.ctx), "cuCtxPushCurrent")? };
        let r = f(self);
        let mut popped: CUcontext = std::ptr::null_mut();
        unsafe { (self.api.cu.ctx_pop)(&mut popped) };
        r
    }

    fn session(&self) -> Res<Session> {
        let of = &self.api.of;
        let mut handle: NvOFHandle = std::ptr::null_mut();
        unsafe {
            self.check(None, (of.create.ok_or("API incompleta")?)(self.ctx, &mut handle), "nvCreateOpticalFlowCuda")?;
            let params = NV_OF_INIT_PARAMS {
                width: self.width,
                height: self.height,
                out_grid_size: NV_OF_OUTPUT_VECTOR_GRID_SIZE_4,
                hint_grid_size: NV_OF_HINT_VECTOR_GRID_SIZE_UNDEFINED,
                mode: NV_OF_MODE_OPTICALFLOW,
                perf_level: NV_OF_PERF_LEVEL_MEDIUM,
                enable_external_hints: NV_OF_FALSE,
                enable_output_cost: NV_OF_FALSE,
                h_priv_data: std::ptr::null_mut(),
                disparity_range: 0,
                enable_roi: NV_OF_FALSE,
            };
            let mut s = Session { handle, inputs: [Buffer::null(), Buffer::null()], output: Buffer::null() };
            // A partir daqui, se algo falhar, `s` é destruída pelo chamador via Drop do OpticalFlow.
            if let Err(e) = self.check(Some(handle), (of.init.ok_or("API incompleta")?)(handle, &params), "nvOFInit") {
                (of.destroy.ok_or("API incompleta")?)(handle);
                return Err(e);
            }
            let input = NV_OF_BUFFER_DESCRIPTOR {
                width: self.width,
                height: self.height,
                buffer_usage: NV_OF_BUFFER_USAGE_INPUT,
                buffer_format: NV_OF_BUFFER_FORMAT_GRAYSCALE8,
            };
            let output = NV_OF_BUFFER_DESCRIPTOR {
                width: self.out_w,
                height: self.out_h,
                buffer_usage: NV_OF_BUFFER_USAGE_OUTPUT,
                buffer_format: NV_OF_BUFFER_FORMAT_SHORT2,
            };
            let made = (|| -> Res<()> {
                s.inputs[0] = self.buffer(handle, &input)?;
                s.inputs[1] = self.buffer(handle, &input)?;
                s.output = self.buffer(handle, &output)?;
                Ok(())
            })();
            if let Err(e) = made {
                self.destroy_session(&mut s);
                return Err(e);
            }
            Ok(s)
        }
    }

    unsafe fn buffer(&self, handle: NvOFHandle, desc: &NV_OF_BUFFER_DESCRIPTOR) -> Res<Buffer> {
        let of = &self.api.of;
        let mut b: NvOFGPUBufferHandle = std::ptr::null_mut();
        self.check(
            Some(handle),
            (of.create_buffer.ok_or("API incompleta")?)(handle, desc, NV_OF_CUDA_BUFFER_TYPE_CUDEVICEPTR, &mut b),
            "nvOFCreateGPUBufferCuda",
        )?;
        let ptr = (of.buffer_get_devptr.ok_or("API incompleta")?)(b);
        let mut info = NV_OF_CUDA_BUFFER_STRIDE_INFO::default();
        self.check(Some(handle), (of.buffer_get_stride.ok_or("API incompleta")?)(b, &mut info), "nvOFGPUBufferGetStrideInfo")?;
        Ok(Buffer { handle: b, ptr, stride: info.stride_info[0].stride_x_in_bytes as usize })
    }

    fn destroy_session(&self, s: &mut Session) {
        let of = &self.api.of;
        unsafe {
            for b in s.inputs.iter_mut().chain(std::iter::once(&mut s.output)) {
                if !b.handle.is_null() {
                    if let Some(f) = of.destroy_buffer {
                        f(b.handle);
                    }
                    b.handle = std::ptr::null_mut();
                }
            }
            if !s.handle.is_null() {
                if let Some(f) = of.destroy {
                    f(s.handle);
                }
                s.handle = std::ptr::null_mut();
            }
        }
    }

    fn check(&self, handle: Option<NvOFHandle>, status: NV_OF_STATUS, what: &str) -> Res<()> {
        if status == NV_OF_SUCCESS {
            return Ok(());
        }
        let mut detail = String::new();
        if let (Some(h), Some(f)) = (handle, self.api.of.get_last_error) {
            let mut buf = [0u8; 256];
            let mut size = buf.len() as u32;
            unsafe { f(h, buf.as_mut_ptr(), &mut size) };
            detail = String::from_utf8_lossy(&buf[..(size as usize).min(buf.len())]).trim_end_matches('\0').to_string();
        }
        Err(format!("{what}: erro {status} {detail}").trim().to_string())
    }

    /// Envia o luma do quadro novo. Se havia um anterior, devolve o fluxo entre os dois.
    pub fn push(&mut self, luma: &[u8], stride: usize) -> Res<Option<FlowPair>> {
        if luma.len() < stride * self.height as usize || stride < self.width as usize {
            return Err("quadro menor que o esperado".into());
        }
        self.with_ctx(|of| {
            let slot = of.newest.map(|n| 1 - n).unwrap_or(0);
            for s in [&of.forward, &of.backward] {
                of.upload(luma, stride, &s.inputs[slot])?;
            }
            let previous = of.newest.replace(slot);
            let Some(prev) = previous else { return Ok(None) };
            // Ida: entrada = anterior, referência = novo. Volta: o contrário.
            let forward = of.execute(&of.forward, prev, slot)?;
            let backward = of.execute(&of.backward, slot, prev)?;
            Ok(Some(FlowPair { forward, backward }))
        })
    }

    /// Esquece o quadro anterior (o próximo `push` não gera fluxo).
    pub fn reset(&mut self) {
        self.newest = None;
    }

    fn upload(&self, luma: &[u8], stride: usize, dst: &Buffer) -> Res<()> {
        let copy = CUDA_MEMCPY2D {
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: luma.as_ptr() as usize,
            src_pitch: stride,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: dst.ptr,
            dst_pitch: dst.stride,
            width_in_bytes: self.width as usize,
            height: self.height as usize,
            ..Default::default()
        };
        unsafe { check_cu((self.api.cu.memcpy_2d)(&copy), "cuMemcpy2D (envio)") }
    }

    fn execute(&self, s: &Session, input: usize, reference: usize) -> Res<FlowField> {
        let of = &self.api.of;
        let inp = NV_OF_EXECUTE_INPUT_PARAMS {
            input_frame: s.inputs[input].handle,
            reference_frame: s.inputs[reference].handle,
            external_hints: std::ptr::null_mut(),
            disable_temporal_hints: NV_OF_FALSE,
            padding: 0,
            h_priv_data: std::ptr::null_mut(),
            padding2: 0,
            num_rois: 0,
            roi_data: std::ptr::null_mut(),
        };
        let mut out = NV_OF_EXECUTE_OUTPUT_PARAMS {
            output_buffer: s.output.handle,
            output_cost_buffer: std::ptr::null_mut(),
            h_priv_data: std::ptr::null_mut(),
        };
        unsafe { self.check(Some(s.handle), (of.execute.ok_or("API incompleta")?)(s.handle, &inp, &mut out), "nvOFExecute")? };
        let row = self.out_w as usize * 4;
        let mut data = vec![0i16; self.out_w as usize * self.out_h as usize * 2];
        let copy = CUDA_MEMCPY2D {
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_device: s.output.ptr,
            src_pitch: s.output.stride,
            dst_memory_type: CU_MEMORYTYPE_HOST,
            dst_host: data.as_mut_ptr() as usize,
            dst_pitch: row,
            width_in_bytes: row,
            height: self.out_h as usize,
            ..Default::default()
        };
        // Cópia síncrona na stream padrão: espera o nvOFExecute terminar.
        unsafe { check_cu((self.api.cu.memcpy_2d)(&copy), "cuMemcpy2D (fluxo)")? };
        Ok(FlowField { width: self.out_w, height: self.out_h, grid: NV_OF_OUTPUT_VECTOR_GRID_SIZE_4, data })
    }
}

impl Buffer {
    fn null() -> Self {
        Buffer { handle: std::ptr::null_mut(), ptr: 0, stride: 0 }
    }
}

impl Drop for OpticalFlow {
    fn drop(&mut self) {
        let _ = self.with_ctx(|of| {
            let (mut f, mut b) = (
                std::mem::replace(&mut of.forward, Session { handle: std::ptr::null_mut(), inputs: [Buffer::null(), Buffer::null()], output: Buffer::null() }),
                std::mem::replace(&mut of.backward, Session { handle: std::ptr::null_mut(), inputs: [Buffer::null(), Buffer::null()], output: Buffer::null() }),
            );
            of.destroy_session(&mut f);
            of.destroy_session(&mut b);
            Ok(())
        });
        if !self.ctx.is_null() {
            unsafe { (self.api.cu.ctx_destroy)(self.ctx) };
        }
    }
}

fn check_cu(r: CUresult, what: &str) -> Res<()> {
    if r == 0 { Ok(()) } else { Err(format!("{what}: erro CUDA {r}")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn hash(x: i32, y: i32) -> u32 {
        let mut n = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263);
        n = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
        n ^ (n >> 16)
    }

    /// Ruído suave (interpolado) numa grade de `cell` pixels: sem repetição em nenhuma escala.
    fn value_noise(x: i32, y: i32, cell: i32) -> f32 {
        let (cx, cy) = (x.div_euclid(cell), y.div_euclid(cell));
        let (fx, fy) = (x.rem_euclid(cell) as f32 / cell as f32, y.rem_euclid(cell) as f32 / cell as f32);
        let v = |i, j| (hash(cx + i, cy + j) & 255) as f32 / 255.0;
        let top = v(0, 0) + (v(1, 0) - v(0, 0)) * fx;
        let bottom = v(0, 1) + (v(1, 1) - v(0, 1)) * fx;
        top + (bottom - top) * fy
    }

    /// Textura com detalhe em várias escalas (como uma imagem real), deslocada.
    fn pattern(w: usize, h: usize, shift_x: i32, shift_y: i32) -> Vec<u8> {
        let mut v = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = (x as i32 - shift_x, y as i32 - shift_y);
                let n = value_noise(sx, sy, 64) * 110.0 + value_noise(sx, sy, 16) * 70.0 + value_noise(sx, sy, 4) * 50.0;
                v[y * w + x] = (n as i32 + 10).clamp(0, 255) as u8;
            }
        }
        v
    }

    /// Placa NVIDIA de verdade: o quadro B é o A deslocado (+12, -6) px. Confere o sentido
    /// dos vetores (é nisso que o shader de interpolação se baseia) e o tempo.
    /// `cargo test nvof -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn nvof_measures_a_known_shift() {
        let (w, h) = (1920usize, 1080usize);
        let mut of = OpticalFlow::new(w as u32, h as u32).expect("NVOF");
        let a = pattern(w, h, 0, 0);
        let b = pattern(w, h, 12, -6); // conteúdo de A andou 12 px para a direita e 6 para cima
        assert!(of.push(&a, w).unwrap().is_none(), "primeiro quadro não tem par");
        let t = Instant::now();
        let pair = of.push(&b, w).unwrap().expect("par");
        println!("fluxo nos dois sentidos (1080p): {:?}", t.elapsed());
        let (fx, fy) = pair.forward.at(960.0, 540.0);
        let (bx, by) = pair.backward.at(960.0, 540.0);
        println!("ida: ({fx:.2}, {fy:.2})  volta: ({bx:.2}, {by:.2})  inconsistência: {:.1}%", pair.inconsistency() * 100.0);
        assert!((fx - 12.0).abs() < 1.0 && (fy + 6.0).abs() < 1.0, "ida devia ser (+12, -6): A(p) = B(p + v)");
        assert!((bx + 12.0).abs() < 1.0 && (by - 6.0).abs() < 1.0, "volta devia ser (-12, +6): B(p) = A(p + v)");
        assert!(pair.inconsistency() < 0.1);

        // Repetido, só o tempo do NVOF (envio do luma + ida + volta + cópia dos vetores).
        let frames: Vec<Vec<u8>> = (0..10).map(|i| pattern(w, h, 12 + i * 3, -6)).collect();
        let t = Instant::now();
        for f in frames.iter().cycle().take(60) {
            of.push(f, w).unwrap();
        }
        let per = t.elapsed() / 60;
        println!("média por quadro (60 quadros): {per:?}");
        assert!(per.as_millis() < 20, "lento demais para 30 fps com folga: {per:?}");
    }
}
