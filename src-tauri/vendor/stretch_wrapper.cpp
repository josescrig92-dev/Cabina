// Minimal C ABI over Signalsmith Stretch (MIT), used for keylock / master tempo.
// Buffers are planar: one float* per channel.
#include "signalsmith-stretch/signalsmith-stretch.h"
#include <cstddef>

using Stretcher = signalsmith::stretch::SignalsmithStretch<float>;

extern "C" {

void *cabina_stretch_create(int channels, float sample_rate) {
    auto *s = new Stretcher(42);
    // splitComputation spreads the FFT work evenly across callbacks (no CPU spikes on the audio thread)
    s->presetDefault(channels, sample_rate, true);
    return s;
}

void cabina_stretch_destroy(void *h) { delete static_cast<Stretcher *>(h); }

void cabina_stretch_reset(void *h) { static_cast<Stretcher *>(h)->reset(); }

int cabina_stretch_input_latency(void *h) { return static_cast<Stretcher *>(h)->inputLatency(); }

int cabina_stretch_output_latency(void *h) { return static_cast<Stretcher *>(h)->outputLatency(); }

int cabina_stretch_block_samples(void *h) { return static_cast<Stretcher *>(h)->blockSamples(); }

int cabina_stretch_interval_samples(void *h) { return static_cast<Stretcher *>(h)->intervalSamples(); }

void cabina_stretch_seek(void *h, const float *const *input, int input_samples, double rate) {
    static_cast<Stretcher *>(h)->seek(input, input_samples, rate);
}

void cabina_stretch_process(void *h, const float *const *input, int input_samples, float *const *output,
                            int output_samples) {
    static_cast<Stretcher *>(h)->process(input, input_samples, output, output_samples);
}
}
