#include "protocol.hpp"
#include "whisper.h"
#include <cmath>
#include <cstring>
#include <memory>

static void silent_log(enum ggml_log_level, const char *, void *) {}

int main() {
    whisper_log_set(silent_log, nullptr);
    voice::Runtime runtime;
    runtime.start("asr");
    using Context = std::unique_ptr<whisper_context, decltype(&whisper_free)>;
    Context context(nullptr, whisper_free);
    std::vector<float> pcm;
    int threads = 2;
    for (;;) {
        auto command = runtime.next();
        std::string job;
        try {
            if (command.binary) {
                if (!context) throw voice::Failure{"model_not_loaded"};
                if (!pcm.empty()) throw voice::Failure{"pcm_already_supplied"};
                if (command.pcm.empty() || command.pcm.size() % 4 || command.pcm.size() / 4 > 960000)
                    throw voice::Failure{"invalid_pcm"};
                pcm.resize(command.pcm.size() / 4);
                for (size_t i = 0; i < pcm.size(); ++i) {
                    const auto * p = command.pcm.data() + i * 4;
                    const uint32_t bits = uint32_t(p[0]) | (uint32_t(p[1]) << 8U)
                        | (uint32_t(p[2]) << 16U) | (uint32_t(p[3]) << 24U);
                    std::memcpy(&pcm[i], &bits, sizeof(bits));
                    if (!std::isfinite(pcm[i]) || std::abs(pcm[i]) > 1.001F)
                        throw voice::Failure{"invalid_pcm"};
                }
                continue;
            }
            const auto op = command.data.at("op").get<std::string>();
            if (op == "shutdown") {
                runtime.send({{"event", "bye"}});
                std::_Exit(0);
            }
            if (op == "unload") {
                pcm.clear(); pcm.shrink_to_fit(); context.reset();
                runtime.send({{"event", "unloaded"}});
            } else if (op == "load") {
                if (context) throw voice::Failure{"already_loaded"};
                const auto path = voice::model_path_from(command.data);
                threads = voice::threads_from(command.data);
                if (command.data.value("gpu_layers", 0) != 0) throw voice::Failure{"gpu_not_available"};
                auto parameters = whisper_context_default_params();
                parameters.use_gpu = false;
                context.reset(whisper_init_from_file_with_params(path.c_str(), parameters));
                if (!context) throw voice::Failure{"model_load_failed", true};
                if (!whisper_is_multilingual(context.get())) {
                    context.reset();
                    throw voice::Failure{"asr_model_not_multilingual"};
                }
                runtime.send({{"event", "ready"}, {"backend", "cpu"}});
            } else if (op == "run") {
                job = command.data.at("job_id").get<std::string>();
                if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                if (!context) throw voice::Failure{"model_not_loaded"};
                if (pcm.empty()) throw voice::Failure{"pcm_missing"};
                const auto language = command.data.value("language", std::string("auto"));
                if (language != "auto" && language != "zh" && language != "en")
                    throw voice::Failure{"invalid_language"};
                auto parameters = whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
                parameters.n_threads = threads;
                parameters.language = language.c_str();
                parameters.translate = false;
                parameters.no_context = true;
                parameters.print_special = false;
                parameters.print_progress = false;
                parameters.print_realtime = false;
                parameters.print_timestamps = false;
                parameters.abort_callback = voice::Runtime::should_abort;
                parameters.abort_callback_user_data = &runtime;
                const int status = whisper_full(context.get(), parameters, pcm.data(), static_cast<int>(pcm.size()));
                pcm.clear(); pcm.shrink_to_fit();
                if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                if (status != 0) throw voice::Failure{"asr_failed", true};
                std::string text;
                const int count = whisper_full_n_segments(context.get());
                for (int i = 0; i < count; ++i) {
                    text += whisper_full_get_segment_text(context.get(), i);
                    if (text.size() > 128 * 1024) throw voice::Failure{"asr_output_too_large"};
                }
                runtime.send({{"event", "result"}, {"job_id", job}, {"text", text}});
                runtime.finish_job();
            }
        } catch (const voice::Failure & failure) {
            pcm.clear(); pcm.shrink_to_fit();
            if (failure.code == "cancelled") runtime.acknowledge_cancelled(job);
            else runtime.error(failure.code, job, failure.retryable);
            runtime.finish_job();
        } catch (...) {
            pcm.clear(); pcm.shrink_to_fit();
            runtime.error("invalid_request", job, false);
            runtime.finish_job();
        }
    }
}
