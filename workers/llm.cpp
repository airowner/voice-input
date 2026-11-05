#include "protocol.hpp"
#include "llama.h"
#include "output_grammar.hpp"
#include "greedy_grammar.hpp"
#include <memory>

static void silent_log(enum ggml_log_level, const char *, void *) {}

int main() {
    llama_log_set(silent_log, nullptr);
    llama_backend_init();
    voice::Runtime runtime;
    runtime.start("llm");
    using Model = std::unique_ptr<llama_model, decltype(&llama_model_free)>;
    using Context = std::unique_ptr<llama_context, decltype(&llama_free)>;
#ifdef VOICE_GRAMMAR_REFERENCE
    using Sampler = std::unique_ptr<llama_sampler, decltype(&llama_sampler_free)>;
#endif
    Model model(nullptr, llama_model_free);
    Context context(nullptr, llama_free);
    std::vector<llama_token> previous_prompt;
    const auto clear_cache = [&] {
        previous_prompt.clear();
        if (context && llama_get_memory(context.get()))
            llama_memory_clear(llama_get_memory(context.get()), true);
    };
    for (;;) {
        auto command = runtime.next();
        std::string job;
        try {
            if (command.binary) throw voice::Failure{"unexpected_pcm"};
            const auto op = command.data.at("op").get<std::string>();
            if (op == "shutdown") {
                runtime.send({{"event", "bye"}});
                std::_Exit(0);
            }
            if (op == "unload") {
                clear_cache();
                context.reset(); model.reset();
                runtime.send({{"event", "unloaded"}});
            } else if (op == "load") {
                if (model) throw voice::Failure{"already_loaded"};
                const auto path = voice::model_path_from(command.data);
                const int threads = voice::threads_from(command.data);
                const int context_tokens = command.data.value("context_tokens", 4096);
                if (context_tokens < 2048 || context_tokens > 16384) throw voice::Failure{"invalid_context"};
                if (command.data.value("gpu_layers", 0) != 0) throw voice::Failure{"gpu_not_available"};
                auto model_parameters = llama_model_default_params();
                model_parameters.n_gpu_layers = 0;
                model.reset(llama_model_load_from_file(path.c_str(), model_parameters));
                if (!model) throw voice::Failure{"model_load_failed", true};
                const char * setup_template = llama_model_chat_template(model.get(), nullptr);
                if (!setup_template || !*setup_template) {
                    model.reset(); throw voice::Failure{"chat_template_missing"};
                }
                const llama_chat_message setup_messages[] = {
                    {"system", "Voice input configuration check."},
                    {"user", "{\"text\":\"configuration test\"}"},
                };
                const int32_t setup_size = llama_chat_apply_template(setup_template, setup_messages, 2, true, nullptr, 0);
                if (setup_size <= 0 || setup_size > 131072) {
                    model.reset(); throw voice::Failure{"chat_template_unsupported"};
                }
                if (context_tokens > llama_model_n_ctx_train(model.get())) {
                    model.reset(); throw voice::Failure{"context_above_model_limit"};
                }
                auto context_parameters = llama_context_default_params();
                context_parameters.n_ctx = static_cast<uint32_t>(context_tokens);
                context_parameters.n_batch = 512;
                context_parameters.n_ubatch = 128;
                context_parameters.n_threads = threads;
                context_parameters.n_threads_batch = threads;
                context_parameters.offload_kqv = false;
                context_parameters.op_offload = false;
                context_parameters.no_perf = true;
                context_parameters.abort_callback = voice::Runtime::should_abort;
                context_parameters.abort_callback_data = &runtime;
                context.reset(llama_init_from_model(model.get(), context_parameters));
                if (!context) {
                    model.reset(); throw voice::Failure{"context_load_failed", true};
                }
                previous_prompt.clear();
                runtime.send({{"event", "ready"}, {"backend", "cpu"}, {"context_tokens", llama_n_ctx(context.get())}});
            } else if (op == "run") {
                job = command.data.at("job_id").get<std::string>();
                if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                if (!context || !model) throw voice::Failure{"model_not_loaded"};
                const int max_tokens = command.data.value("max_tokens", 768);
                if (max_tokens < 1 || max_tokens > 1024) throw voice::Failure{"invalid_output_budget"};
                const auto & messages = command.data.at("messages");
                if (!messages.is_array() || messages.size() != 2) throw voice::Failure{"invalid_messages"};
                std::vector<std::string> contents;
                size_t bytes = 0;
                for (size_t i = 0; i < messages.size(); ++i) {
                    const auto role = messages[i].at("role").get<std::string>();
                    if (role != (i == 0 ? "system" : "user")) throw voice::Failure{"invalid_messages"};
                    contents.push_back(messages[i].at("content").get<std::string>());
                    if (contents.back().find('\0') != std::string::npos)
                        throw voice::Failure{"invalid_messages"};
                    bytes += contents.back().size();
                }
                if (bytes > 32768) throw voice::Failure{"input_too_large"};
                const llama_chat_message chat[] = {{"system", contents[0].c_str()}, {"user", contents[1].c_str()}};
                const char * chat_template = llama_model_chat_template(model.get(), nullptr);
                if (!chat_template) throw voice::Failure{"chat_template_missing"};
                std::vector<char> prompt(bytes * 2 + 4096);
                int32_t prompt_size = llama_chat_apply_template(chat_template, chat, 2, true, prompt.data(), static_cast<int32_t>(prompt.size()));
                if (prompt_size < 0) throw voice::Failure{"chat_template_unsupported"};
                if (static_cast<size_t>(prompt_size) > prompt.size()) {
                    if (prompt_size > 131072) throw voice::Failure{"input_too_large"};
                    prompt.resize(static_cast<size_t>(prompt_size) + 1);
                    prompt_size = llama_chat_apply_template(chat_template, chat, 2, true, prompt.data(), static_cast<int32_t>(prompt.size()));
                }
                if (prompt_size < 0 || static_cast<size_t>(prompt_size) > prompt.size())
                    throw voice::Failure{"chat_template_unsupported"};
                const auto * vocab = llama_model_get_vocab(model.get());
                const int32_t required = llama_tokenize(vocab, prompt.data(), prompt_size, nullptr, 0, true, true);
                if (required >= 0 || required == INT32_MIN) throw voice::Failure{"tokenization_failed"};
                const auto token_count = static_cast<size_t>(-required);
                if (token_count + static_cast<size_t>(max_tokens) > llama_n_ctx(context.get()))
                    throw voice::Failure{"context_budget_exceeded"};
                std::vector<llama_token> tokens(token_count);
                if (llama_tokenize(vocab, prompt.data(), prompt_size, tokens.data(), static_cast<int32_t>(tokens.size()), true, true) < 0)
                    throw voice::Failure{"tokenization_failed"};
                auto memory = llama_get_memory(context.get());
                if (!memory) throw voice::Failure{"model_memory_unavailable"};
                size_t retained = 0;
#ifndef VOICE_GRAMMAR_REFERENCE
                while (retained < previous_prompt.size() && retained < tokens.size()
                        && previous_prompt[retained] == tokens[retained]) ++retained;
                // Never reuse logits from the preceding job: re-evaluate at
                // least the last prompt token and remove every generated token.
                retained = std::min(retained, tokens.size() - 1);
                // Only reuse complete prefill blocks. KV from a prior partial
                // ubatch can differ from a fresh prompt's batch layout.
                retained -= retained % 128;
                if (retained == 0 || !llama_memory_seq_rm(memory, 0, static_cast<llama_pos>(retained), -1)) {
                    llama_memory_clear(memory, true);
                    retained = 0;
                }
#else
                llama_memory_clear(memory, true);
#endif
#if defined(VOICE_CACHE_DIAGNOSTIC) || defined(VOICE_GRAMMAR_REFERENCE)
                const auto prefill_started = std::chrono::steady_clock::now();
#endif
                for (size_t offset = retained; offset < tokens.size(); offset += 128) {
                    if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                    auto batch = llama_batch_get_one(tokens.data() + offset, static_cast<int32_t>(std::min<size_t>(128, tokens.size() - offset)));
                    if (llama_decode(context.get(), batch) != 0) {
                        if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                        throw voice::Failure{"prompt_decode_failed", true};
                    }
                }
#if defined(VOICE_CACHE_DIAGNOSTIC) || defined(VOICE_GRAMMAR_REFERENCE)
                const double prefill_ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - prefill_started).count();
#endif
#ifdef VOICE_GRAMMAR_REFERENCE
                Sampler sampler(llama_sampler_chain_init(llama_sampler_chain_default_params()), llama_sampler_free);
                if (!sampler) throw voice::Failure{"sampler_failed", true};
                auto * grammar = llama_sampler_init_grammar(vocab, voice_output_grammar, "root");
                auto * greedy = llama_sampler_init_greedy();
                if (!grammar || !greedy) {
                    if (grammar) llama_sampler_free(grammar);
                    if (greedy) llama_sampler_free(greedy);
                    throw voice::Failure{"output_grammar_failed", true};
                }
                llama_sampler_chain_add(sampler.get(), grammar);
                llama_sampler_chain_add(sampler.get(), greedy);
#else
                voice::GreedyGrammar sampler(vocab);
#endif
                std::string text;
                bool finished = false;
                for (int i = 0; i < max_tokens; ++i) {
                    if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
#ifdef VOICE_GRAMMAR_REFERENCE
                    auto token = llama_sampler_sample(sampler.get(), context.get(), -1);
#else
                    auto token = sampler.sample(context.get());
#endif
                    if (llama_vocab_is_eog(vocab, token)) { finished = true; break; }
                    std::vector<char> piece(256);
                    int32_t length = llama_token_to_piece(vocab, token, piece.data(), static_cast<int32_t>(piece.size()), 0, false);
                    if (length < 0) {
                        if (length == INT32_MIN || -length > 8192) throw voice::Failure{"invalid_token_piece"};
                        piece.resize(static_cast<size_t>(-length));
                        length = llama_token_to_piece(vocab, token, piece.data(), static_cast<int32_t>(piece.size()), 0, false);
                    }
                    if (length < 0) throw voice::Failure{"invalid_token_piece"};
                    text.append(piece.data(), static_cast<size_t>(length));
                    if (text.size() > 128 * 1024) throw voice::Failure{"output_too_large"};
                    auto batch = llama_batch_get_one(&token, 1);
                    if (llama_decode(context.get(), batch) != 0) {
                        if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                        throw voice::Failure{"generation_failed", true};
                    }
                }
                if (runtime.cancelled.load()) throw voice::Failure{"cancelled"};
                if (!finished) throw voice::Failure{"output_truncated", true};
                previous_prompt = std::move(tokens);
                voice::json response = {{"event", "result"}, {"job_id", job}, {"text", text}};
#if defined(VOICE_CACHE_DIAGNOSTIC) || defined(VOICE_GRAMMAR_REFERENCE)
                response["cache_probe"] = {{"retained_prompt_tokens", retained}, {"prompt_tokens", token_count}, {"prefill_ms", prefill_ms}};
#endif
                runtime.send(response);
                runtime.finish_job();
            }
        } catch (const voice::Failure & failure) {
            clear_cache();
            if (failure.code == "cancelled") runtime.acknowledge_cancelled(job);
            else runtime.error(failure.code, job, failure.retryable);
            runtime.finish_job();
        } catch (...) {
            clear_cache();
            runtime.error("invalid_request", job, false);
            runtime.finish_job();
        }
    }
}
