//! Envoltorio seguro sobre Signalsmith Stretch (keylock / master tempo).

use std::ffi::c_void;

extern "C" {
    fn cabina_stretch_create(channels: i32, sample_rate: f32) -> *mut c_void;
    fn cabina_stretch_destroy(h: *mut c_void);
    fn cabina_stretch_reset(h: *mut c_void);
    fn cabina_stretch_input_latency(h: *mut c_void) -> i32;
    fn cabina_stretch_output_latency(h: *mut c_void) -> i32;
    fn cabina_stretch_block_samples(h: *mut c_void) -> i32;
    fn cabina_stretch_interval_samples(h: *mut c_void) -> i32;
    fn cabina_stretch_seek(h: *mut c_void, input: *const *const f32, n: i32, rate: f64);
    fn cabina_stretch_process(h: *mut c_void, input: *const *const f32, nin: i32, output: *const *mut f32, nout: i32);
}

pub struct Stretcher {
    h: *mut c_void,
    pub in_lat: usize,
    pub out_lat: usize,
    pub preroll: usize,
}

// El puntero solo se usa desde un hilo a la vez (el hilo de audio).
unsafe impl Send for Stretcher {}

impl Stretcher {
    pub fn new(sample_rate: f64) -> Self {
        unsafe {
            let h = cabina_stretch_create(2, sample_rate as f32);
            let in_lat = cabina_stretch_input_latency(h) as usize;
            let out_lat = cabina_stretch_output_latency(h) as usize;
            let preroll = (cabina_stretch_block_samples(h) + cabina_stretch_interval_samples(h)) as usize;
            Self { h, in_lat, out_lat, preroll }
        }
    }

    pub fn reset(&mut self) {
        unsafe { cabina_stretch_reset(self.h) }
    }

    pub fn seek(&mut self, l: &[f32], r: &[f32], rate: f64) {
        debug_assert_eq!(l.len(), r.len());
        let ptrs = [l.as_ptr(), r.as_ptr()];
        unsafe { cabina_stretch_seek(self.h, ptrs.as_ptr(), l.len() as i32, rate) }
    }

    pub fn process(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        let ins = [in_l.as_ptr(), in_r.as_ptr()];
        let outs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
        unsafe {
            cabina_stretch_process(self.h, ins.as_ptr(), in_l.len() as i32, outs.as_ptr(), out_l.len() as i32)
        }
    }
}

impl Drop for Stretcher {
    fn drop(&mut self) {
        unsafe { cabina_stretch_destroy(self.h) }
    }
}
