//! Codificação e decodificação H.264 com Media Foundation (vem com o Windows).
//!
//! Encoder: BGRA → (Video Processor MFT: conversão + escala) → NV12 → encoder H.264.
//! Usa o encoder de hardware da GPU (NVENC / AMF / Quick Sync expostos como MFTs);
//! se não houver, cai no encoder de software da Microsoft.
//! Decoder: H.264 (Annex B) → NV12.

#![allow(clippy::too_many_arguments)]

use std::mem::ManuallyDrop;
use windows::core::{Interface, GUID};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

pub type Res<T> = Result<T, String>;

pub(crate) fn e<E: std::fmt::Display>(ctx: &'static str) -> impl Fn(E) -> String {
    move |err| format!("{ctx}: {err}")
}

/// Inicializa COM + Media Foundation na thread atual (pode chamar várias vezes).
pub fn mf_init() -> Res<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(0x0002_0070, MFSTARTUP_FULL).map_err(e("MFStartup"))
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub(crate) fn video_type(subtype: &GUID, w: u32, h: u32, fps: u32) -> Res<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().map_err(e("MFCreateMediaType"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("major"))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype).map_err(e("subtype"))?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(e("size"))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(e("fps"))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(e("par"))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(e("interlace"))?;
        Ok(t)
    }
}

fn find_mft(category: GUID, flags: MFT_ENUM_FLAG, input: GUID, output: GUID) -> Vec<(IMFActivate, String)> {
    let inp = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: input };
    let out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: output };
    let mut list = Vec::new();
    unsafe {
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        if MFTEnumEx(category, flags, Some(&inp), Some(&out), &mut arr, &mut n).is_ok() && !arr.is_null() {
            for i in 0..n as usize {
                if let Some(act) = (*arr.add(i)).take() {
                    let name = friendly_name(&act);
                    list.push((act, name));
                }
            }
            CoTaskMemFree(Some(arr as *const _));
        }
    }
    list
}

fn friendly_name(act: &IMFActivate) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let mut len = 0u32;
        if act.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
            return String::from_utf16_lossy(&buf[..len as usize]);
        }
    }
    "MFT".into()
}

fn set_codec_value(codec: &ICodecAPI, api: &GUID, value: VARIANT) -> bool {
    unsafe { codec.SetValue(api, &value).is_ok() }
}

fn new_sample(size: u32) -> Res<IMFSample> {
    unsafe {
        let sample = MFCreateSample().map_err(e("MFCreateSample"))?;
        let buf = MFCreateMemoryBuffer(size).map_err(e("MFCreateMemoryBuffer"))?;
        sample.AddBuffer(&buf).map_err(e("AddBuffer"))?;
        Ok(sample)
    }
}

fn sample_from_bytes(data: &[u8], time_hns: i64, dur_hns: i64) -> Res<IMFSample> {
    unsafe {
        let buf = MFCreateMemoryBuffer(data.len() as u32).map_err(e("MFCreateMemoryBuffer"))?;
        let mut ptr = std::ptr::null_mut();
        buf.Lock(&mut ptr, None, None).map_err(e("Lock"))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buf.Unlock().map_err(e("Unlock"))?;
        buf.SetCurrentLength(data.len() as u32).map_err(e("SetCurrentLength"))?;
        let sample = MFCreateSample().map_err(e("MFCreateSample"))?;
        sample.AddBuffer(&buf).map_err(e("AddBuffer"))?;
        sample.SetSampleTime(time_hns).map_err(e("SetSampleTime"))?;
        sample.SetSampleDuration(dur_hns).map_err(e("SetSampleDuration"))?;
        Ok(sample)
    }
}

fn sample_bytes(sample: &IMFSample) -> Res<Vec<u8>> {
    unsafe {
        let buf = sample.ConvertToContiguousBuffer().map_err(e("ConvertToContiguousBuffer"))?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut ptr, None, Some(&mut len)).map_err(e("Lock"))?;
        let out = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buf.Unlock().map_err(e("Unlock"))?;
        Ok(out)
    }
}

/// Chama ProcessOutput uma vez. `Ok(None)` = precisa de mais entrada.
/// `Err("STREAM_CHANGE")` = o formato de saída mudou (decoder).
fn pull_output(mft: &IMFTransform, provides: bool, alloc: u32) -> Res<Option<IMFSample>> {
    unsafe {
        let sample = if provides { None } else { Some(new_sample(alloc)?) };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = mft.ProcessOutput(0, &mut buffers, &mut status);
        let out = ManuallyDrop::take(&mut buffers[0].pSample);
        drop(ManuallyDrop::take(&mut buffers[0].pEvents));
        match result {
            Ok(()) => Ok(out),
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => Err("STREAM_CHANGE".into()),
            Err(err) => Err(format!("ProcessOutput: {err}")),
        }
    }
}

fn stream_info(mft: &IMFTransform) -> (bool, u32) {
    unsafe {
        match mft.GetOutputStreamInfo(0) {
            Ok(info) => {
                let provides = info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                    != 0;
                (provides, info.cbSize.max(1))
            }
            Err(_) => (true, 1),
        }
    }
}

/* ====================================================================== */
/*                       Conversão BGRA → NV12 + escala                    */
/* ====================================================================== */

/// Video Processor MFT: BGRA (tamanho da captura) → NV12 (tamanho de saída).
pub struct Converter {
    mft: IMFTransform,
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
    provides: bool,
    alloc: u32,
}

impl Converter {
    pub fn new(in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Res<Self> {
        unsafe {
            let mft: IMFTransform = windows::Win32::System::Com::CoCreateInstance(
                &CLSID_VideoProcessorMFT,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )
            .map_err(e("VideoProcessorMFT"))?;
            let input = video_type(&MFVideoFormat_RGB32, in_w, in_h, fps)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, in_w * 4).map_err(e("stride"))?;
            mft.SetInputType(0, &input, 0).map_err(e("VP SetInputType"))?;
            let output = video_type(&MFVideoFormat_NV12, out_w, out_h, fps)?;
            mft.SetOutputType(0, &output, 0).map_err(e("VP SetOutputType"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            let (provides, mut alloc) = stream_info(&mft);
            alloc = alloc.max(out_w * out_h * 3 / 2);
            Ok(Self { mft, in_w, in_h, out_w, out_h, provides, alloc })
        }
    }

    pub fn input_size(&self) -> (u32, u32) {
        (self.in_w, self.in_h)
    }

    /// Converte um quadro BGRA (sem padding) em uma amostra NV12.
    pub fn convert(&self, bgra: &[u8], time_hns: i64, dur_hns: i64) -> Res<IMFSample> {
        let sample = sample_from_bytes(bgra, time_hns, dur_hns)?;
        unsafe { self.mft.ProcessInput(0, &sample, 0).map_err(e("VP ProcessInput"))? };
        // Drena até o MFT pedir mais entrada; senão ele recusa o próximo quadro.
        let mut last = None;
        while let Some(s) = pull_output(&self.mft, self.provides, self.alloc)? {
            last = Some(s);
        }
        let out = last.ok_or("VP sem saída")?;
        unsafe {
            out.SetSampleTime(time_hns).ok();
            out.SetSampleDuration(dur_hns).ok();
        }
        Ok(out)
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }
}

/* ====================================================================== */
/*                               Encoder                                    */
/* ====================================================================== */

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub time_hns: i64,
}

pub struct Encoder {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    provides: bool,
    alloc: u32,
    pub name: String,
    pub hardware: bool,
    sequence_header: Vec<u8>,
    pending_input: u32,
}

impl Encoder {
    /// Cria o encoder H.264 (hardware primeiro). `bitrate` em bits/s.
    pub fn new(w: u32, h: u32, fps: u32, bitrate: u32, prefer_hardware: bool, d3d: Option<&IMFDXGIDeviceManager>) -> Res<Self> {
        let mut candidates = Vec::new();
        if prefer_hardware {
            candidates.extend(find_mft(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                MFVideoFormat_NV12,
                MFVideoFormat_H264,
            ).into_iter().map(|c| (c, true)));
        }
        candidates.extend(find_mft(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            MFVideoFormat_NV12,
            MFVideoFormat_H264,
        ).into_iter().map(|c| (c, false)));

        let mut last_err = "nenhum encoder H.264 encontrado".to_string();
        for ((act, name), hardware) in candidates {
            // Com textura da GPU, só serve encoder de hardware que aceite D3D11.
            if d3d.is_some() && !hardware {
                continue;
            }
            match Self::open(&act, &name, hardware, w, h, fps, bitrate, d3d) {
                Ok(enc) => return Ok(enc),
                Err(err) => last_err = format!("{name}: {err}"),
            }
        }
        Err(last_err)
    }

    fn open(act: &IMFActivate, name: &str, hardware: bool, w: u32, h: u32, fps: u32, bitrate: u32, d3d: Option<&IMFDXGIDeviceManager>) -> Res<Self> {
        unsafe {
            let mft: IMFTransform = act.ActivateObject().map_err(e("ActivateObject"))?;
            let mut events = None;
            if let Ok(attrs) = mft.GetAttributes() {
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(e("ASYNC_UNLOCK"))?;
                    events = Some(mft.cast::<IMFMediaEventGenerator>().map_err(e("events"))?);
                }
                attrs.SetUINT32(&MF_LOW_LATENCY, 1).ok();
            }
            if let Some(m) = d3d {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize).map_err(e("SET_D3D_MANAGER"))?;
            }
            let codec = mft.cast::<ICodecAPI>().ok();
            let configure = |c: &ICodecAPI| {
                set_codec_value(c, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                set_codec_value(c, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                set_codec_value(c, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                set_codec_value(c, &CODECAPI_AVEncCommonBufferSize, VARIANT::from(bitrate)); // 1 s de buffer
                set_codec_value(c, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * 4)); // keyframe a cada ~4 s
                set_codec_value(c, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // Deixa o encoder comprimir até o limite do H.264 quando a cena é muito pesada.
                set_codec_value(c, &CODECAPI_AVEncVideoMaxQP, VARIANT::from(51u32));
            };
            if let Some(c) = &codec {
                configure(c);
            }

            let out = video_type(&MFVideoFormat_H264, w, h, fps)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).map_err(e("bitrate"))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).ok();
            mft.SetOutputType(0, &out, 0).map_err(e("SetOutputType"))?;
            let inp = video_type(&MFVideoFormat_NV12, w, h, fps)?;
            mft.SetInputType(0, &inp, 0).map_err(e("SetInputType"))?;
            // Alguns encoders zeram o controle de taxa ao definir os formatos: reaplica.
            if let Some(c) = &codec {
                configure(c);
            }

            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok();
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(e("BEGIN_STREAMING"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(e("START_OF_STREAM"))?;

            let (provides, alloc) = stream_info(&mft);
            let sequence_header = mft
                .GetOutputCurrentType(0)
                .ok()
                .and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER))
                .unwrap_or_default();

            Ok(Self {
                mft, codec, events, provides,
                alloc: alloc.max(w * h),
                name: name.to_string(),
                hardware,
                sequence_header,
                pending_input: 0,
            })
        }
    }

    /// Muda o bitrate ao vivo (bits/s).
    pub fn set_bitrate(&self, bitrate: u32) {
        if let Some(c) = &self.codec {
            set_codec_value(c, &CODECAPI_AVEncCommonBufferSize, VARIANT::from(bitrate));
            set_codec_value(c, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
        }
    }

    /// Pede que o próximo quadro seja um keyframe (alguém entrou ou perdeu pacotes).
    pub fn force_keyframe(&self) {
        if let Some(c) = &self.codec {
            set_codec_value(c, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
        }
    }

    /// Codifica uma amostra NV12 e devolve os pacotes prontos (pode ser 0, 1 ou mais).
    pub fn encode(&mut self, nv12: &IMFSample) -> Res<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        match self.events.clone() {
            None => {
                unsafe { self.mft.ProcessInput(0, nv12, 0).map_err(e("ProcessInput"))? };
                while let Some(s) = pull_output(&self.mft, self.provides, self.alloc)? {
                    out.push(self.packet(&s)?);
                }
            }
            Some(events) => {
                // MFT assíncrono (hardware): espera pedir entrada; recolhe saídas no caminho.
                let mut fed = false;
                loop {
                    let ev = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) }.map_err(e("GetEvent"))?;
                    let ty = unsafe { ev.GetType() }.map_err(e("GetType"))?;
                    if ty == METransformNeedInput.0 as u32 {
                        if fed {
                            self.pending_input += 1;
                            break;
                        }
                        unsafe { self.mft.ProcessInput(0, nv12, 0).map_err(e("ProcessInput"))? };
                        fed = true;
                        if self.pending_input > 0 {
                            self.pending_input -= 1;
                            break;
                        }
                    } else if ty == METransformHaveOutput.0 as u32 {
                        if let Some(s) = pull_output(&self.mft, self.provides, self.alloc)? {
                            out.push(self.packet(&s)?);
                        }
                        if fed {
                            break;
                        }
                    }
                }
                if !fed {
                    return Err("encoder não pediu entrada".into());
                }
            }
        }
        Ok(out)
    }

    fn packet(&mut self, s: &IMFSample) -> Res<EncodedFrame> {
        let mut data = sample_bytes(s)?;
        let keyframe = unsafe { s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 };
        let time_hns = unsafe { s.GetSampleTime().unwrap_or(0) };
        if self.sequence_header.is_empty() {
            if let Some(h) = unsafe { self.mft.GetOutputCurrentType(0) }.ok().and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER)) {
                self.sequence_header = h;
            }
        }
        // Garante SPS/PPS antes de cada keyframe (quem entra no meio precisa deles).
        if keyframe && !has_nal(&data, 7) && !self.sequence_header.is_empty() {
            let mut with = self.sequence_header.clone();
            with.extend_from_slice(&data);
            data = with;
        }
        Ok(EncodedFrame { data, keyframe, time_hns })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0).ok();
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0).ok();
            if let Some(ev) = &self.events {
                let _ = ev.cast::<IMFShutdown>().map(|s| s.Shutdown());
            }
        }
    }
}

fn blob(t: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    unsafe {
        let size = t.GetBlobSize(key).ok()?;
        let mut buf = vec![0u8; size as usize];
        t.GetBlob(key, &mut buf, None).ok()?;
        Some(buf)
    }
}

/// Procura um NAL do tipo `kind` num fluxo Annex B.
pub fn has_nal(data: &[u8], kind: u8) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && (data[i + 2] == 1 || (data[i + 2] == 0 && data[i + 3] == 1)) {
            let start = if data[i + 2] == 1 { i + 3 } else { i + 4 };
            if start < data.len() && data[start] & 0x1f == kind {
                return true;
            }
            i = start;
        } else {
            i += 1;
        }
    }
    false
}

/// Controle de bitrate em malha fechada: o encoder do Media Foundation erra o alvo
/// (passa 10–30% com conteúdo complexo). A cada segundo, corrige o valor pedido
/// com base no que realmente saiu. Nunca pede mais que o alvo.
pub fn adjust_bitrate(effective: u32, target: u32, measured: u32) -> u32 {
    if measured == 0 || target == 0 {
        return target;
    }
    let (eff, tgt, got) = (effective as f64, target as f64, measured as f64);
    let next = if got > tgt * 1.03 {
        eff * (tgt / got).powf(0.8) // passou: reduz
    } else if got < tgt * 0.92 && got > eff * 0.85 {
        eff * (tgt / got).powf(0.5) // ficou abaixo usando tudo que pediu: sobe aos poucos
    } else {
        eff
    };
    // Piso de 50%: se a cena está no limite de compressão, baixar mais só piora depois.
    next.clamp(tgt * 0.5, tgt) as u32
}

/// Nova amostra apontando para o mesmo buffer, com outro horário (repetir quadro).
pub fn rewrap(sample: &IMFSample, time_hns: i64, dur_hns: i64) -> Res<IMFSample> {
    unsafe {
        let buf = sample.GetBufferByIndex(0).map_err(e("GetBufferByIndex"))?;
        let s = MFCreateSample().map_err(e("MFCreateSample"))?;
        s.AddBuffer(&buf).map_err(e("AddBuffer"))?;
        s.SetSampleTime(time_hns).map_err(e("SetSampleTime"))?;
        s.SetSampleDuration(dur_hns).map_err(e("SetSampleDuration"))?;
        Ok(s)
    }
}

/* ====================================================================== */
/*                               Decoder                                    */
/* ====================================================================== */

/// Quadro NV12 decodificado (sem padding de altura; `stride` ≥ `width`).
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>, // Y (stride*height) seguido de UV (stride*height/2)
}

pub struct Decoder {
    mft: IMFTransform,
    provides: bool,
    alloc: u32,
    width: u32,
    height: u32,
    stride: u32,
    buf_height: u32,
}

impl Decoder {
    pub fn new() -> Res<Self> {
        unsafe {
            let candidates = find_mft(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                MFVideoFormat_H264,
                MFVideoFormat_NV12,
            );
            let (act, _) = candidates.into_iter().next().ok_or("decoder H.264 não encontrado")?;
            let mft: IMFTransform = act.ActivateObject().map_err(e("ActivateObject"))?;
            if let Ok(attrs) = mft.GetAttributes() {
                attrs.SetUINT32(&MF_LOW_LATENCY, 1).ok();
            }
            if let Ok(codec) = mft.cast::<ICodecAPI>() {
                set_codec_value(&codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            }
            let inp = MFCreateMediaType().map_err(e("MFCreateMediaType"))?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(e("major"))?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(e("subtype"))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_MixedInterlaceOrProgressive.0 as u32).ok();
            mft.SetInputType(0, &inp, 0).map_err(e("SetInputType"))?;
            let mut dec = Self { mft, provides: false, alloc: 1, width: 0, height: 0, stride: 0, buf_height: 0 };
            dec.select_output()?;
            dec.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ok();
            dec.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ok();
            Ok(dec)
        }
    }

    fn select_output(&mut self) -> Res<()> {
        unsafe {
            let mut i = 0;
            loop {
                let t = self.mft.GetOutputAvailableType(0, i).map_err(e("GetOutputAvailableType"))?;
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    self.mft.SetOutputType(0, &t, 0).map_err(e("SetOutputType"))?;
                    let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                    let (bw, bh) = ((size >> 32) as u32, size as u32);
                    self.buf_height = bh;
                    self.stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(bw as i32).unsigned_abs().max(bw);
                    // A área visível pode ser menor que o buffer (ex.: 1080 de 1088).
                    match blob(&t, &MF_MT_MINIMUM_DISPLAY_APERTURE) {
                        Some(b) if b.len() >= std::mem::size_of::<MFVideoArea>() => {
                            let area = &*(b.as_ptr() as *const MFVideoArea);
                            self.width = area.Area.cx as u32;
                            self.height = area.Area.cy as u32;
                        }
                        _ => {
                            self.width = bw;
                            self.height = bh;
                        }
                    }
                    let (provides, alloc) = stream_info(&self.mft);
                    self.provides = provides;
                    self.alloc = alloc.max(self.stride * self.buf_height * 3 / 2);
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    /// Decodifica um pacote Annex B; devolve os quadros prontos.
    pub fn decode(&mut self, data: &[u8], time_hns: i64) -> Res<Vec<Nv12Frame>> {
        let sample = sample_from_bytes(data, time_hns, 0)?;
        let mut frames = Vec::new();
        unsafe {
            if let Err(err) = self.mft.ProcessInput(0, &sample, 0) {
                if err.code() != MF_E_NOTACCEPTING {
                    return Err(format!("ProcessInput: {err}"));
                }
                // O decoder está cheio: esvazia a saída e tenta de novo. Descartar o quadro
                // aqui corrompe todos os seguintes até o próximo keyframe (a imagem só
                // atualiza onde há movimento).
                self.drain_into(&mut frames)?;
                self.mft.ProcessInput(0, &sample, 0).map_err(|e| format!("ProcessInput: {e}"))?;
            }
        }
        self.drain_into(&mut frames)?;
        Ok(frames)
    }

    fn drain_into(&mut self, frames: &mut Vec<Nv12Frame>) -> Res<()> {
        loop {
            match pull_output(&self.mft, self.provides, self.alloc) {
                Ok(Some(s)) => frames.push(self.frame(&s)?),
                Ok(None) => break,
                Err(err) if err == "STREAM_CHANGE" => self.select_output()?,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn frame(&self, s: &IMFSample) -> Res<Nv12Frame> {
        let raw = sample_bytes(s)?;
        let (w, h, stride, bh) = (self.width, self.height, self.stride as usize, self.buf_height as usize);
        let y_size = stride * h as usize;
        let mut data = Vec::with_capacity(y_size * 3 / 2);
        data.extend_from_slice(raw.get(..y_size).ok_or("quadro curto")?);
        let uv_start = stride * bh;
        let uv_size = stride * (h as usize / 2);
        data.extend_from_slice(raw.get(uv_start..uv_start + uv_size).ok_or("quadro curto")?);
        Ok(Nv12Frame { width: w, height: h, stride: self.stride, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// O bitrate muda de verdade durante a transmissão?
    #[test]
    #[ignore]
    fn live_bitrate_change() {
        mf_init().unwrap();
        let (w, h, fps) = (1280u32, 720u32, 60u32);
        let conv = Converter::new(w, h, w, h, fps).unwrap();
        let mut enc = Encoder::new(w, h, fps, 6_000_000, true, None).unwrap();
        let frames: Vec<Vec<u8>> = (0..8).map(|t| gradient(w, h, t)).collect();
        for sec in 0..6u32 {
            if sec == 3 { enc.set_bitrate(2_000_000); }
            let mut bytes = 0usize;
            for f in 0..fps {
                let i = sec * fps + f;
                let t = i as i64 * 10_000_000 / fps as i64;
                let nv12 = conv.convert(&frames[(i % 8) as usize], t, 10_000_000 / fps as i64).unwrap();
                for p in enc.encode(&nv12).unwrap() { bytes += p.data.len(); }
            }
            println!("segundo {sec}: alvo {} Mb/s, saiu {:.2} Mb/s", if sec < 3 { 6 } else { 2 }, (bytes * 8) as f64 / 1e6);
        }
    }

    /// Com conteúdo complexo, a malha fechada traz o bitrate para perto do alvo.
    #[test]
    #[ignore]
    fn bitrate_feedback_converges() {
        mf_init().unwrap();
        let (w, h, fps, target) = (1920u32, 1080u32, 60u32, 6_000_000u32);
        let conv = Converter::new(w, h, w, h, fps).unwrap();
        let mut enc = Encoder::new(w, h, fps, target, true, None).unwrap();
        let mut eff = target;
        let mut img = vec![0u8; (w * h * 4) as usize];
        let mut last = 0.0;
        for sec in 0..8u32 {
            let mut bytes = 0usize;
            for f in 0..fps {
                let i = sec * fps + f;
                for (j, px) in img.chunks_exact_mut(4).enumerate() {
                    let (x, y) = (j as u32 % w, j as u32 / w);
                    px[0] = ((x + i * 9) ^ (y * 3)) as u8; px[1] = ((y + i * 5) ^ x) as u8; px[2] = (i * 3) as u8; px[3] = 255;
                }
                let t = i as i64 * 10_000_000 / fps as i64;
                let nv12 = conv.convert(&img, t, 10_000_000 / fps as i64).unwrap();
                for p in enc.encode(&nv12).unwrap() { bytes += p.data.len(); }
            }
            let got = (bytes * 8) as u32;
            last = got as f64 / 1e6;
            println!("segundo {sec}: pedido {:.2} Mb/s, saiu {:.2} Mb/s (alvo 6)", eff as f64 / 1e6, last);
            eff = adjust_bitrate(eff, target, got);
            enc.set_bitrate(eff);
        }
        println!("último segundo: {last:.2} Mb/s (conteúdo-limite: o encoder não comprime abaixo de ~7,5)");
    }

    fn gradient(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                v[i] = ((x + t * 4) % 256) as u8;
                v[i + 1] = ((y + t * 2) % 256) as u8;
                v[i + 2] = 128;
                v[i + 3] = 255;
            }
        }
        v
    }

    #[test]
    fn encode_decode_roundtrip() {
        mf_init().unwrap();
        let (cw, ch, w, h, fps) = (1920, 1080, 1280, 720, 60);
        let conv = Converter::new(cw, ch, w, h, fps).expect("converter");
        let mut enc = Encoder::new(w, h, fps, 6_000_000, true, None).expect("encoder");
        println!("encoder: {} (hardware: {})", enc.name, enc.hardware);
        let mut dec = Decoder::new().expect("decoder");

        let frames: Vec<Vec<u8>> = (0..8).map(|t| gradient(cw, ch, t)).collect();
        let (mut packets, mut decoded, mut bytes) = (0, 0, 0);
        let mut first_key = None;
        let mut max_lag = 0i64;
        let start = Instant::now();
        for i in 0..120u32 {
            let t = i as i64 * 10_000_000 / fps as i64;
            let nv12 = conv.convert(&frames[(i % 8) as usize], t, 10_000_000 / fps as i64).expect("convert");
            if i == 60 { enc.force_keyframe(); }
            for p in enc.encode(&nv12).expect("encode") {
                packets += 1;
                bytes += p.data.len();
                if first_key.is_none() { first_key = Some(p.keyframe && has_nal(&p.data, 7)); }
                for f in dec.decode(&p.data, p.time_hns).expect("decode") {
                    assert_eq!((f.width, f.height), (w, h));
                    decoded += 1;
                }
                max_lag = max_lag.max(packets as i64 - decoded as i64);
            }
        }
        let el = start.elapsed();
        println!("120 quadros em {el:?} ({:.0} fps), pacotes {packets}, decodificados {decoded}, atraso máx {max_lag} quadros, {} KB",
            120.0 / el.as_secs_f64(), bytes / 1024);
        assert_eq!(first_key, Some(true), "primeiro pacote precisa ser keyframe com SPS");
        assert!(packets >= 100, "poucos pacotes: {packets}");
        assert!(max_lag <= 2, "decoder segurando quadros demais: {max_lag}");
    }
}
